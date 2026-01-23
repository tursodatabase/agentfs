package agentfs

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"time"
)

// ToolCalls provides tool call tracking backed by SQLite.
type ToolCalls struct {
	db *sql.DB
}

// PendingCall represents an in-progress tool call.
type PendingCall struct {
	tc        *ToolCalls
	name      string
	params    json.RawMessage
	startedAt int64
}

// Start begins tracking a tool call.
// Returns a PendingCall that should be completed with Success() or Error().
func (tc *ToolCalls) Start(ctx context.Context, name string, parameters any) (*PendingCall, error) {
	var params json.RawMessage
	if parameters != nil {
		var err error
		params, err = json.Marshal(parameters)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal parameters: %w", err)
		}
	}

	return &PendingCall{
		tc:        tc,
		name:      name,
		params:    params,
		startedAt: time.Now().Unix(),
	}, nil
}

// Success marks the pending call as successful and records it.
func (pc *PendingCall) Success(ctx context.Context, result any) (*ToolCall, error) {
	var resultJSON json.RawMessage
	if result != nil {
		var err error
		resultJSON, err = json.Marshal(result)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal result: %w", err)
		}
	}

	completedAt := time.Now().Unix()
	durationMs := (completedAt - pc.startedAt) * 1000

	var id int64
	var paramsPtr *string
	if pc.params != nil {
		s := string(pc.params)
		paramsPtr = &s
	}

	var resultPtr *string
	if resultJSON != nil {
		s := string(resultJSON)
		resultPtr = &s
	}

	err := pc.tc.db.QueryRowContext(ctx, toolCallsInsert,
		pc.name, paramsPtr, resultPtr, nil, pc.startedAt, completedAt, durationMs,
	).Scan(&id)
	if err != nil {
		return nil, fmt.Errorf("failed to record tool call: %w", err)
	}

	return &ToolCall{
		ID:          id,
		Name:        pc.name,
		Parameters:  pc.params,
		Result:      resultJSON,
		Error:       nil,
		StartedAt:   pc.startedAt,
		CompletedAt: completedAt,
		DurationMs:  durationMs,
	}, nil
}

// Error marks the pending call as failed and records it.
func (pc *PendingCall) Error(ctx context.Context, err error) (*ToolCall, error) {
	completedAt := time.Now().Unix()
	durationMs := (completedAt - pc.startedAt) * 1000

	var paramsPtr *string
	if pc.params != nil {
		s := string(pc.params)
		paramsPtr = &s
	}

	errStr := err.Error()

	var id int64
	queryErr := pc.tc.db.QueryRowContext(ctx, toolCallsInsert,
		pc.name, paramsPtr, nil, errStr, pc.startedAt, completedAt, durationMs,
	).Scan(&id)
	if queryErr != nil {
		return nil, fmt.Errorf("failed to record tool call: %w", queryErr)
	}

	return &ToolCall{
		ID:          id,
		Name:        pc.name,
		Parameters:  pc.params,
		Result:      nil,
		Error:       &errStr,
		StartedAt:   pc.startedAt,
		CompletedAt: completedAt,
		DurationMs:  durationMs,
	}, nil
}

// Record inserts a complete tool call record directly.
// This is an alternative to the Start/Success/Error pattern.
func (tc *ToolCalls) Record(ctx context.Context, name string, parameters, result any, errMsg *string, startedAt, completedAt int64) (*ToolCall, error) {
	var paramsJSON json.RawMessage
	if parameters != nil {
		var err error
		paramsJSON, err = json.Marshal(parameters)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal parameters: %w", err)
		}
	}

	var resultJSON json.RawMessage
	if result != nil {
		var err error
		resultJSON, err = json.Marshal(result)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal result: %w", err)
		}
	}

	durationMs := (completedAt - startedAt) * 1000

	var paramsPtr *string
	if paramsJSON != nil {
		s := string(paramsJSON)
		paramsPtr = &s
	}

	var resultPtr *string
	if resultJSON != nil {
		s := string(resultJSON)
		resultPtr = &s
	}

	var id int64
	err := tc.db.QueryRowContext(ctx, toolCallsInsert,
		name, paramsPtr, resultPtr, errMsg, startedAt, completedAt, durationMs,
	).Scan(&id)
	if err != nil {
		return nil, fmt.Errorf("failed to record tool call: %w", err)
	}

	return &ToolCall{
		ID:          id,
		Name:        name,
		Parameters:  paramsJSON,
		Result:      resultJSON,
		Error:       errMsg,
		StartedAt:   startedAt,
		CompletedAt: completedAt,
		DurationMs:  durationMs,
	}, nil
}

// Get retrieves a tool call by ID.
func (tc *ToolCalls) Get(ctx context.Context, id int64) (*ToolCall, error) {
	var call ToolCall
	var params, result, errStr sql.NullString

	err := tc.db.QueryRowContext(ctx, toolCallsGetByID, id).Scan(
		&call.ID, &call.Name, &params, &result, &errStr,
		&call.StartedAt, &call.CompletedAt, &call.DurationMs,
	)
	if err == sql.ErrNoRows {
		return nil, fmt.Errorf("tool call not found: %d", id)
	}
	if err != nil {
		return nil, fmt.Errorf("failed to get tool call: %w", err)
	}

	if params.Valid {
		call.Parameters = json.RawMessage(params.String)
	}
	if result.Valid {
		call.Result = json.RawMessage(result.String)
	}
	if errStr.Valid {
		call.Error = &errStr.String
	}

	return &call, nil
}

// GetByName retrieves tool calls by name.
func (tc *ToolCalls) GetByName(ctx context.Context, name string, limit int) ([]ToolCall, error) {
	if limit <= 0 {
		limit = 100
	}

	rows, err := tc.db.QueryContext(ctx, toolCallsGetByName, name, limit)
	if err != nil {
		return nil, fmt.Errorf("failed to query tool calls: %w", err)
	}
	defer rows.Close()

	return scanToolCalls(rows)
}

// GetRecent retrieves recent tool calls (since timestamp).
func (tc *ToolCalls) GetRecent(ctx context.Context, since int64, limit int) ([]ToolCall, error) {
	if limit <= 0 {
		limit = 100
	}

	rows, err := tc.db.QueryContext(ctx, toolCallsGetRecent, since, limit)
	if err != nil {
		return nil, fmt.Errorf("failed to query tool calls: %w", err)
	}
	defer rows.Close()

	return scanToolCalls(rows)
}

// GetStats returns aggregated statistics for tool calls.
func (tc *ToolCalls) GetStats(ctx context.Context) ([]ToolCallStats, error) {
	rows, err := tc.db.QueryContext(ctx, toolCallsGetStats)
	if err != nil {
		return nil, fmt.Errorf("failed to query stats: %w", err)
	}
	defer rows.Close()

	var stats []ToolCallStats
	for rows.Next() {
		var s ToolCallStats
		if err := rows.Scan(&s.Name, &s.TotalCalls, &s.Successful, &s.Failed, &s.AvgDurationMs); err != nil {
			return nil, err
		}
		stats = append(stats, s)
	}

	return stats, rows.Err()
}

// scanToolCalls scans rows into a slice of ToolCall
func scanToolCalls(rows *sql.Rows) ([]ToolCall, error) {
	var calls []ToolCall
	for rows.Next() {
		var call ToolCall
		var params, result, errStr sql.NullString

		if err := rows.Scan(
			&call.ID, &call.Name, &params, &result, &errStr,
			&call.StartedAt, &call.CompletedAt, &call.DurationMs,
		); err != nil {
			return nil, err
		}

		if params.Valid {
			call.Parameters = json.RawMessage(params.String)
		}
		if result.Valid {
			call.Result = json.RawMessage(result.String)
		}
		if errStr.Valid {
			call.Error = &errStr.String
		}

		calls = append(calls, call)
	}

	return calls, rows.Err()
}
