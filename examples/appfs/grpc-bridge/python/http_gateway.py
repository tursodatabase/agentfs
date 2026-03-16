#!/usr/bin/env python3
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

import grpc

import appfs_adapter_v1_pb2 as pb2
import appfs_adapter_v1_pb2_grpc as pb2_grpc


def _resp(handler: BaseHTTPRequestHandler, status: int, body: dict) -> None:
    payload = json.dumps(body).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(payload)))
    handler.end_headers()
    handler.wfile.write(payload)


def _to_input_mode(mode: str) -> pb2.InputMode:
    mode = (mode or "").strip().lower()
    if mode == "text":
        return pb2.INPUT_MODE_TEXT
    if mode == "json":
        return pb2.INPUT_MODE_JSON
    if mode == "text_or_json":
        return pb2.INPUT_MODE_TEXT_OR_JSON
    return pb2.INPUT_MODE_UNSPECIFIED


def _to_execution_mode(mode: str) -> pb2.ExecutionMode:
    mode = (mode or "").strip().lower()
    if mode == "inline":
        return pb2.EXECUTION_MODE_INLINE
    if mode == "streaming":
        return pb2.EXECUTION_MODE_STREAMING
    return pb2.EXECUTION_MODE_UNSPECIFIED


def _ctx_from_json(data: dict) -> pb2.RequestContext:
    ctx = data.get("context", {}) or {}
    return pb2.RequestContext(
        app_id=str(ctx.get("app_id", "")),
        session_id=str(ctx.get("session_id", "")),
        request_id=str(ctx.get("request_id", "")),
        client_token=str(ctx.get("client_token", "")),
    )


def _parse_json_text(text: str):
    if text is None or text == "":
        return None
    return json.loads(text)


class GatewayHandler(BaseHTTPRequestHandler):
    stub: pb2_grpc.AppfsAdapterBridgeStub

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            data = json.loads(raw.decode("utf-8"))
        except json.JSONDecodeError:
            _resp(
                self,
                400,
                {
                    "kind": "rejected",
                    "code": "INVALID_ARGUMENT",
                    "message": "invalid json body",
                    "retryable": False,
                },
            )
            return

        if self.path == "/v1/submit-action":
            self.handle_submit_action(data)
            return
        if self.path == "/v1/submit-control-action":
            self.handle_submit_control_action(data)
            return

        _resp(self, 404, {"kind": "internal", "message": f"unknown path: {self.path}"})

    def handle_submit_action(self, data: dict) -> None:
        req = pb2.SubmitActionRequest(
            app_id=str(data.get("app_id", "")),
            path=str(data.get("path", "")),
            payload=str(data.get("payload", "")),
            input_mode=_to_input_mode(str(data.get("input_mode", ""))),
            execution_mode=_to_execution_mode(str(data.get("execution_mode", ""))),
            context=_ctx_from_json(data),
        )
        reply = self.stub.SubmitAction(req)
        which = reply.WhichOneof("result")

        if which == "completed":
            content = _parse_json_text(reply.completed.content_json)
            _resp(self, 200, {"kind": "completed", "content": content})
            return
        if which == "streaming":
            plan = {
                "terminal_content": _parse_json_text(reply.streaming.terminal_content_json),
            }
            if reply.streaming.has_accepted_content:
                plan["accepted_content"] = _parse_json_text(
                    reply.streaming.accepted_content_json
                )
            if reply.streaming.has_progress_content:
                plan["progress_content"] = _parse_json_text(
                    reply.streaming.progress_content_json
                )
            _resp(self, 200, {"kind": "streaming", "plan": plan})
            return
        if which == "error":
            _resp(
                self,
                400,
                {
                    "kind": "rejected",
                    "code": reply.error.code,
                    "message": reply.error.message,
                    "retryable": reply.error.retryable,
                },
            )
            return

        _resp(
            self,
            500,
            {"kind": "internal", "message": "invalid bridge reply: no result"},
        )

    def handle_submit_control_action(self, data: dict) -> None:
        action = data.get("action", {}) or {}
        kind = str(action.get("kind", ""))
        req = pb2.SubmitControlActionRequest(
            app_id=str(data.get("app_id", "")),
            path=str(data.get("path", "")),
            context=_ctx_from_json(data),
        )
        if kind == "paging_fetch_next":
            req.paging_fetch_next.handle_id = str(action.get("handle_id", ""))
            req.paging_fetch_next.page_no = int(action.get("page_no", 0))
            req.paging_fetch_next.has_more = bool(action.get("has_more", False))
        elif kind == "paging_close":
            req.paging_close.handle_id = str(action.get("handle_id", ""))
        else:
            _resp(
                self,
                400,
                {
                    "kind": "rejected",
                    "code": "NOT_SUPPORTED",
                    "message": f"unsupported control action: {kind}",
                    "retryable": False,
                },
            )
            return

        reply = self.stub.SubmitControlAction(req)
        which = reply.WhichOneof("result")
        if which == "completed":
            content = _parse_json_text(reply.completed.content_json)
            _resp(self, 200, {"kind": "completed", "content": content})
            return
        if which == "error":
            _resp(
                self,
                400,
                {
                    "kind": "rejected",
                    "code": reply.error.code,
                    "message": reply.error.message,
                    "retryable": reply.error.retryable,
                },
            )
            return
        _resp(
            self,
            500,
            {"kind": "internal", "message": "invalid bridge reply: no result"},
        )

    def log_message(self, format: str, *args) -> None:
        return


def main() -> None:
    channel = grpc.insecure_channel("127.0.0.1:50051")
    GatewayHandler.stub = pb2_grpc.AppfsAdapterBridgeStub(channel)
    server = HTTPServer(("127.0.0.1", 8080), GatewayHandler)
    print("AppFS HTTP gateway listening on http://127.0.0.1:8080 -> gRPC 127.0.0.1:50051")
    server.serve_forever()


if __name__ == "__main__":
    main()
