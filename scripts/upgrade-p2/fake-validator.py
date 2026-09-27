#!/usr/bin/env python3
"""Minimal deterministic stand-in for the Sculpin validation service (upgrade qualification only).

Implements the ledger-facing contract of docs/design/sculpin-validation-service.md
(`sculpin-validation-request/v1` -> `sculpin-validation-response/v1`) without any semantics:
every candidate conforms, the effective context echoes the requested hints (or fixed
defaults), the report digest is derived from the candidate identity. Unknown request fields
are accepted and ignored (the ledger may add fields such as `invocation_id`). Every call is
appended as one JSON line to the log file (request keys, headers of interest, candidate).

Usage: fake-validator.py <bind-host> <port> <log-file>
Not a Sculpin implementation and never a deployment artifact.
"""
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOST, PORT, LOG = sys.argv[1], int(sys.argv[2]), sys.argv[3]
SERVICE_VERSION = "fake-2026.09.27"
CONFIGURATION_VERSION = "fake-cfg-1"


def effective_context(requested):
    requested = requested or {}
    ctx = {
        "base_kb": requested.get("base_kb") or {"kb_id": "urn:fake:kb:default", "revision": "fake-kbrev-1"},
        "shapes": requested.get("shapes") or {"id": "urn:fake:shapes:default", "version": "1+shapes_hash:fake"},
        "virtual_contexts": [],
        "validator": {"service_version": SERVICE_VERSION, "configuration_version": CONFIGURATION_VERSION},
    }
    if requested.get("ontology"):
        ctx["ontology"] = requested["ontology"]
    if requested.get("reasoning_profile"):
        ctx["reasoning"] = {"profile": requested["reasoning_profile"], "implementation": "fake-reasoner", "version": "0.0.1"}
    if requested.get("sources_revision"):
        ctx["sources_revision"] = requested["sources_revision"]
    return ctx


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet stderr; the JSON log is the record
        pass

    def reply(self, status, body):
        data = json.dumps(body, sort_keys=True).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.reply(200, {"status": "ok"}) if self.path == "/health" else self.reply(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length)
        entry = {"path": self.path, "idempotency_key": self.headers.get("idempotency-key"),
                 "correlation": self.headers.get("x-correlation-id"),
                 "authorization_present": self.headers.get("authorization") is not None}
        try:
            req = json.loads(raw)
            cand = req["candidate"]
            entry.update({"request_keys": sorted(req.keys()), "invocation_id": req.get("invocation_id"),
                          "protocol": req.get("protocol"), "graph_id": cand.get("graph_id"),
                          "commit": cand["commit"], "state_digest": cand["state_digest"],
                          "quads": len(cand.get("quads") or [])})
            if self.path != "/validate" or req.get("protocol") != "sculpin-validation-request/v1":
                entry["answer"] = 400
                self.reply(400, {"error": "unsupported path or protocol"})
                return
            body = {
                "protocol": "sculpin-validation-response/v1",
                "candidate_commit": cand["commit"],
                "candidate_state_digest": cand["state_digest"],
                "context": effective_context(req.get("requested")),
                "outcome": {"kind": "conforms", "violation_count": 0, "violations": []},
                "report": {"digest": "sha256:" + hashlib.sha256(("fake-report\n" + cand["commit"] + "\n" + cand["state_digest"]).encode()).hexdigest(),
                           "reference": "urn:fake:validation-report:" + cand["commit"].split(":")[-1][:16]},
            }
            entry["answer"] = 200
            self.reply(200, body)
        except Exception as e:  # malformed request: the ledger must never send one
            entry.update({"answer": 400, "error": repr(e)})
            self.reply(400, {"error": "malformed request"})
        finally:
            with open(LOG, "a") as f:
                f.write(json.dumps(entry, sort_keys=True) + "\n")


ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
