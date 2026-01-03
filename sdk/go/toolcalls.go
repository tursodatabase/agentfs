package agentfs

type ToolCallStatus string

const (
	Pending ToolCallStatus = "pending"
	Success ToolCallStatus = "success"
	Error   ToolCallStatus = "error"
)

type ToolCall struct {
	id           int
	name         string
	parameters   any
	result       any
	error        *string
	status       ToolCallStatus
	started_at   *int
	completed_at *int
	duration_ms  *int
}

type ToolCallStats struct {
	name            string
	total_calls     int
	successful      int
	failed          int
	avg_duration_ms float64
}
