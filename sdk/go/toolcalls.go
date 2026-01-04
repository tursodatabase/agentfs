package agentfs

import (
	"context"
	"database/sql"
	"encoding/json"
	"math"
	"time"

	_ "github.com/tursodatabase/go-libsql"
)

type ToolCallStatus string

const (
	Pending ToolCallStatus = "pending"
	Success ToolCallStatus = "success"
	Error   ToolCallStatus = "error"
)

type ToolCall struct {
	Id          int
	Name        string
	Parameters  any
	Result      any
	Error       *string
	Status      ToolCallStatus
	StartedAt   *int
	CompletedAt *int
	DurationMs  *int
}

type ToolCallStats struct {
	Name          string
	TotalCalls    int
	Successful    int
	Failed        int
	AvgDurationMs float64
}

type ToolCalls struct {
	db *sql.DB
}

func NewToolCalls(db *sql.DB) *ToolCalls {
	return &ToolCalls{
		db: db,
	}
}

func NewToolCallsFromDatabase(db *sql.DB) *ToolCalls {
	toolCalls := NewToolCalls(db)
	toolCalls.Initialize()
	return toolCalls
}

func (tc *ToolCalls) Initialize() (err error) {
	ctx := context.Background()
	_, err = tc.db.ExecContext(ctx, `
      CREATE TABLE IF NOT EXISTS tool_calls (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        parameters TEXT,
        result TEXT,
        error TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        started_at INTEGER NOT NULL,
        completed_at INTEGER,
        duration_ms INTEGER
      )
    `)
	if err != nil {
		return
	}
	_, err = tc.db.ExecContext(ctx, `
      CREATE INDEX IF NOT EXISTS idx_tool_calls_name
      ON tool_calls(name)
    `)
	if err != nil {
		return
	}

	_, err = tc.db.ExecContext(ctx, `
      CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at
      ON tool_calls(started_at)
    `)
	return
}

func (tc *ToolCalls) Start(name string, parameters any) (id int, err error) {
	var serializedParameters *string
	switch parameters {
	case nil:
		serializedParameters = nil
	default:
		serParams, err := json.Marshal(parameters)
		if err != nil {
			return 0, err
		}
		serParamsStr := string(serParams)
		serializedParameters = &serParamsStr
	}
	startedAt := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	ctx := context.Background()
	stmt, err := tc.db.PrepareContext(ctx, `
      INSERT INTO tool_calls (name, parameters, status, started_at)
      VALUES (?, ?, 'pending', ?)
      RETURNING id
    `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, name, serializedParameters, startedAt)
	err = row.Scan(&id)
	return
}

func (tc *ToolCalls) Success(id int, result any) (err error) {
	var serializedRes *string
	switch result {
	case nil:
		serializedRes = nil
	default:
		serRes, err := json.Marshal(result)
		if err != nil {
			return err
		}
		serResStr := string(serRes)
		serializedRes = &serResStr
	}
	completedAt := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	ctx := context.Background()
	stmt, err := tc.db.PrepareContext(ctx, `SELECT started_at FROM tool_calls WHERE id = ?`)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, id)
	var startedAt int64
	err = row.Scan(&startedAt)
	if err != nil {
		return
	}
	durationMs := (completedAt - startedAt) * 1000
	updateStmt, err := tc.db.PrepareContext(ctx, `
      UPDATE tool_calls
      SET status = 'success', result = ?, completed_at = ?, duration_ms = ?
      WHERE id = ?
    `)
	if err != nil {
		return err
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, serializedRes, completedAt, durationMs, id)
	return
}

func (tc *ToolCalls) Error(id int, errorMsg string) (err error) {
	completedAt := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	ctx := context.Background()
	stmt, err := tc.db.PrepareContext(ctx, `SELECT started_at FROM tool_calls WHERE id = ?`)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, id)
	var startedAt int64
	err = row.Scan(&startedAt)
	if err != nil {
		return
	}
	durationMs := (completedAt - startedAt) * 1000
	updateStmt, err := tc.db.PrepareContext(ctx, `
      UPDATE tool_calls
      SET status = 'error', error = ?, completed_at = ?, duration_ms = ?
      WHERE id = ?
    `)
	if err != nil {
		return err
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, errorMsg, completedAt, durationMs, id)
	return
}

func (tc *ToolCalls) Record(
	name string,
	startedAt,
	completedAt int,
	parameters,
	result any,
	errorMsg *string,
) (id int, err error) {
	var serializedRes *string
	switch result {
	case nil:
		serializedRes = nil
	default:
		serRes, err := json.Marshal(result)
		if err != nil {
			return 0, err
		}
		serResStr := string(serRes)
		serializedRes = &serResStr
	}
	var serializedParameters *string
	switch parameters {
	case nil:
		serializedParameters = nil
	default:
		serParams, err := json.Marshal(parameters)
		if err != nil {
			return 0, err
		}
		serParamsStr := string(serParams)
		serializedParameters = &serParamsStr
	}
	durationMs := (completedAt - startedAt) * 1000
	var status ToolCallStatus
	if errorMsg == nil {
		status = Success
	} else {
		status = Error
	}
	ctx := context.Background()
	stmt, err := tc.db.PrepareContext(ctx, `
      INSERT INTO tool_calls (name, parameters, result, error, status, started_at, completed_at, duration_ms)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?)
      RETURNING id
    `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, name, serializedParameters, serializedRes, errorMsg, status, startedAt, completedAt, durationMs)
	err = row.Scan(&id)
	return
}

func (tc *ToolCalls) Get(id int) (toolCall ToolCall, err error) {
	ctx := context.Background()
	stmt, err := tc.db.PrepareContext(ctx, `
      SELECT * FROM tool_calls WHERE id = ?
    `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, id)
	err = row.Scan(
		&toolCall.Id,
		&toolCall.Name,
		&toolCall.Parameters,
		&toolCall.Error,
		&toolCall.Status,
		&toolCall.StartedAt,
		&toolCall.CompletedAt,
		&toolCall.DurationMs,
	)
	return
}
