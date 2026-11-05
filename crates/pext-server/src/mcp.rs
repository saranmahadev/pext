//! Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Tools are the registry's operations (`txn.add` becomes `txn_add`), filtered
//! by the profile: `read`, `append` or `write`. Admin operations and anything
//! marked local-only are never offered.

use crate::gateway::{
    check_size, err_envelope, instructions, ok_envelope, untrusted_fields, CallFlags, Gateway,
};
use pext_core::Error;
use pext_engine::registry::{self, OpSpec};
use pext_engine::{Actor, Role};
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};

pub const PROTOCOL: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn profile_role(name: &str) -> Option<Role> {
    match name {
        "read" => Some(Role::Read),
        "append" => Some(Role::Append),
        "write" => Some(Role::Write),
        _ => None,
    }
}

pub fn tool_name(op: &str) -> String {
    op.replace('.', "_")
}

pub fn op_for_tool(tool: &str) -> Option<&'static OpSpec> {
    registry::all().iter().find(|s| tool_name(s.name) == tool)
}

fn schema_for(g: &Gateway, spec: &OpSpec) -> Value {
    let mut s = registry::input_schema(spec);
    if spec.mutating {
        if let Some(p) = s.get_mut("properties").and_then(|p| p.as_object_mut()) {
            p.insert(
                "dry_run".into(),
                json!({"type": "boolean", "description": "preview the change without saving anything"}),
            );
            p.insert(
                "idempotency_key".into(),
                json!({"type": "string", "description": "make this write safe to retry"}),
            );
            if g.allow_confirm {
                p.insert(
                    "confirm".into(),
                    json!({"type": "boolean", "description": "confirm a destructive operation after reviewing a dry run"}),
                );
            }
        }
    }
    s
}

fn tool_json(g: &Gateway, spec: &OpSpec) -> Value {
    let mut about = spec.about.to_string();
    if spec.mutating {
        about.push_str(" [writes data; audited and undoable]");
    }
    json!({
        "name": tool_name(spec.name),
        "title": spec.name,
        "description": about,
        "inputSchema": schema_for(g, spec),
        "annotations": {
            "readOnlyHint": !spec.mutating,
            "destructiveHint": false,
            "idempotentHint": !spec.mutating,
            "openWorldHint": false,
        },
    })
}

fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_err(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_result(body: Value, is_error: bool) -> Value {
    let text = body.to_string();
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": body,
        "isError": is_error,
    })
}

fn call_tool(g: &Gateway, actor: &Actor, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "missing tool name".to_string()))?;
    let spec = op_for_tool(name).ok_or((-32602, format!("unknown tool '{name}'")))?;
    let mut args: Map<String, Value> = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err((-32602, "arguments must be an object".to_string())),
    };
    let mut flags = CallFlags::default();
    let mut bad: Option<Error> = None;
    let mut take_bool = |args: &mut Map<String, Value>, key: &str| -> bool {
        match args.remove(key) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => b,
            Some(_) => {
                bad = Some(
                    Error::validation("invalid_argument", format!("'{key}' must be a boolean"))
                        .with_field(key),
                );
                false
            }
        }
    };
    if spec.mutating {
        flags.dry_run = take_bool(&mut args, "dry_run");
        flags.confirm = take_bool(&mut args, "confirm");
        match args.remove("idempotency_key") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => flags.idempotency_key = Some(s),
            Some(_) => {
                bad = Some(
                    Error::validation("invalid_argument", "'idempotency_key' must be a string")
                        .with_field("idempotency_key"),
                )
            }
        }
    }
    if let Some(e) = bad {
        return Ok(tool_result(err_envelope(&e), true));
    }
    match g.call(spec.name, args, &flags, actor) {
        Ok(out) => {
            let mut env = ok_envelope(&out);
            env["untrusted_text_fields"] = json!(untrusted_fields());
            match check_size(&env.to_string()) {
                Ok(()) => Ok(tool_result(env, false)),
                Err(e) => Ok(tool_result(err_envelope(&e), true)),
            }
        }
        Err(e) => Ok(tool_result(err_envelope(&e), true)),
    }
}

/// Handle one JSON-RPC message; `None` means "no response" (a notification).
pub fn handle(g: &Gateway, actor: &Actor, msg: &Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(rpc_err(
            &Value::Null,
            -32600,
            "request must be a JSON object",
        ));
    };
    let id = obj.get("id").cloned();
    let method = obj.get("method").and_then(|m| m.as_str());
    let Some(method) = method else {
        // A response to something we never sent, or garbage.
        return id.map(|i| rpc_err(&i, -32600, "missing method"));
    };
    let Some(id) = id else {
        return None; // notification: initialized, cancelled, ...
    };
    if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(rpc_err(&id, -32600, "jsonrpc must be \"2.0\""));
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let wanted = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL);
            let version = if SUPPORTED.contains(&wanted) {
                wanted
            } else {
                PROTOCOL
            };
            rpc_ok(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "pext", "title": "pext personal finance", "version": pext_engine::ops::system::VERSION},
                    "instructions": instructions(),
                }),
            )
        }
        "ping" => rpc_ok(&id, json!({})),
        "tools/list" => {
            let tools: Vec<Value> = g
                .tools(actor)
                .into_iter()
                .map(|s| tool_json(g, s))
                .collect();
            rpc_ok(&id, json!({"tools": tools}))
        }
        "tools/call" => match call_tool(g, actor, &params) {
            Ok(r) => rpc_ok(&id, r),
            Err((code, m)) => rpc_err(&id, code, &m),
        },
        "resources/list" => rpc_ok(&id, json!({"resources": []})),
        "prompts/list" => rpc_ok(&id, json!({"prompts": []})),
        _ => rpc_err(&id, -32601, &format!("method not found: {method}")),
    })
}

/// Longest line accepted from the client.
const MAX_LINE: usize = 4 * 1024 * 1024;

/// Serve until the input ends.
pub fn serve<R: BufRead, W: Write>(
    g: &Gateway,
    actor: &Actor,
    input: R,
    mut out: W,
) -> std::io::Result<()> {
    use std::io::Read;
    let mut input = input;
    loop {
        let mut line = Vec::new();
        let n = (&mut input)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(());
        }
        if line.len() > MAX_LINE {
            // Drain the rest of the oversized line, then refuse it.
            if line.last() != Some(&b'\n') {
                let mut sink = Vec::new();
                input.read_until(b'\n', &mut sink)?;
            }
            write_line(
                &mut out,
                &rpc_err(&Value::Null, -32600, "message too large"),
            )?;
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(text) {
            Ok(msg) => handle(g, actor, &msg),
            Err(_) => Some(rpc_err(&Value::Null, -32700, "parse error")),
        };
        if let Some(r) = reply {
            write_line(&mut out, &r)?;
        }
    }
}

fn write_line<W: Write>(out: &mut W, v: &Value) -> std::io::Result<()> {
    writeln!(out, "{v}")?;
    out.flush()
}
