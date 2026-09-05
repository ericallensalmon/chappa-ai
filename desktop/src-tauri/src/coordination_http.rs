//! The HTTP shape of the coordination surface: one handler per
//! `ROUTES` row under `/scratchpads…`, `/todos…` and `/todo_comments…` in
//! `control_http.rs`. Handlers are thin — argument parsing, the actor from
//! the request, the typed `CoordError` → status/JSON mapping — and every
//! rule lives in `coordination.rs`.
//!
//! GET routes take their fields as query parameters, POST routes as JSON
//! body keys; both are normalized into one `Args` view so a handler never
//! cares which transport carried a field (query strings arrive as strings,
//! so the accessors parse integers/booleans/comma-lists from text too).
//!
//! Actor attribution: `Params::actor` (the `X-Chappa-Actor` header, set by
//! chappa-ai-mcp from its own `CHAPPA_AI_PROCESS_ID`; absent = `"user"`).

use serde_json::{json, Value};

use crate::control_http::{json, percent_decode, ControlState, Params, Resp};
use crate::coordination::{
    pad_receipt, todo_receipt, CoordError, EditTarget, FindQuery, ReadMode, ResponseMode, Scratchpad,
    ScratchpadListQuery, Todo, TodoListQuery, TodoPatch,
};

/// Request fields from either transport.
pub(crate) struct Args(pub Value);

impl Args {
    /// POST: the JSON body (empty = `{}`); GET: the query string as an
    /// object of strings.
    pub(crate) fn from(query: &str, body: &str) -> Result<Self, Resp> {
        if !body.trim().is_empty() {
            return serde_json::from_str::<Value>(body)
                .map_err(|err| json(400, json!({"error": "invalid", "message": format!("bad body: {err}")})))
                .and_then(|v| {
                    if v.is_object() {
                        Ok(Self(v))
                    } else {
                        Err(json(400, json!({"error": "invalid", "message": "body must be a JSON object"})))
                    }
                });
        }
        let mut map = serde_json::Map::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(percent_decode(k), Value::String(percent_decode(&v.replace('+', " "))));
        }
        Ok(Self(Value::Object(map)))
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    pub(crate) fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub(crate) fn req_str(&self, key: &str) -> Result<String, CoordError> {
        self.str(key)
            .map(str::to_owned)
            .ok_or_else(|| CoordError::Invalid(format!("`{key}` is required")))
    }

    pub(crate) fn i64(&self, key: &str) -> Result<Option<i64>, CoordError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .map(Some)
                .ok_or_else(|| CoordError::Invalid(format!("`{key}` must be an integer"))),
            Some(Value::String(s)) if s.is_empty() => Ok(None),
            Some(Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map(Some)
                .map_err(|_| CoordError::Invalid(format!("`{key}` must be an integer"))),
            Some(_) => Err(CoordError::Invalid(format!("`{key}` must be an integer"))),
        }
    }

    pub(crate) fn req_i64(&self, key: &str) -> Result<i64, CoordError> {
        self.i64(key)?
            .ok_or_else(|| CoordError::Invalid(format!("`{key}` is required")))
    }

    pub(crate) fn usize(&self, key: &str) -> Result<Option<usize>, CoordError> {
        match self.i64(key)? {
            Some(n) if n < 0 => Err(CoordError::Invalid(format!("`{key}` must be >= 0"))),
            Some(n) => Ok(Some(n as usize)),
            None => Ok(None),
        }
    }

    pub(crate) fn bool(&self, key: &str) -> Result<Option<bool>, CoordError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(Some(true)),
                "false" | "0" | "no" | "" => Ok(Some(false)),
                _ => Err(CoordError::Invalid(format!("`{key}` must be a boolean"))),
            },
            Some(_) => Err(CoordError::Invalid(format!("`{key}` must be a boolean"))),
        }
    }

    /// An array of strings, or a comma-separated string (query form).
    pub(crate) fn strings(&self, key: &str) -> Result<Vec<String>, CoordError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| CoordError::Invalid(format!("`{key}` must be an array of strings")))
                })
                .collect(),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()),
            Some(_) => Err(CoordError::Invalid(format!("`{key}` must be an array of strings"))),
        }
    }

    pub(crate) fn ids(&self, key: &str) -> Result<Vec<i64>, CoordError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_i64()
                        .ok_or_else(|| CoordError::Invalid(format!("`{key}` must be an array of integers")))
                })
                .collect(),
            Some(Value::String(s)) => s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<i64>().map_err(|_| CoordError::Invalid(format!("`{key}` must be integers"))))
                .collect(),
            Some(_) => Err(CoordError::Invalid(format!("`{key}` must be an array of integers"))),
        }
    }

    fn mode(&self) -> Result<ResponseMode, CoordError> {
        ResponseMode::parse(self.str("response_mode"))
    }

    /// `project_id` as a scope: `null`/absent → `None`.
    fn project_scope(&self) -> Result<Option<i64>, CoordError> {
        self.i64("project_id")
    }

    /// The scratchpad scope: absent → every project (`None`);
    /// `project_id=global` / `null` on the query → the global pads only
    /// (`Some(None)`); an integer → that project.
    fn pad_scope(&self) -> Result<Option<Option<i64>>, CoordError> {
        Ok(match self.get("project_id") {
            None => None,
            Some(Value::String(s)) if s == "global" || s == "null" => Some(None),
            Some(_) => Some(self.project_scope()?),
        })
    }

    fn confirm(&self, what: &str) -> Result<(), CoordError> {
        if self.bool("confirm")? == Some(true) {
            Ok(())
        } else {
            Err(CoordError::Invalid(format!("{what} requires confirm=true")))
        }
    }
}

fn fail(err: CoordError) -> Resp {
    json(err.status(), err.to_json())
}

/// Run a handler body that yields the reply value.
pub(crate) fn run(query: &str, body: &str, f: impl FnOnce(&Args) -> Result<Value, CoordError>) -> Resp {
    let args = match Args::from(query, body) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match f(&args) {
        Ok(v) => json(200, v),
        Err(err) => fail(err),
    }
}

pub(crate) fn record(params: &Params) -> Result<i64, CoordError> {
    params
        .record
        .ok_or_else(|| CoordError::Invalid("missing record id".into()))
}

fn line_count(pad: &Scratchpad) -> Value {
    json!(crate::coordination::lines_of(&pad.content).len())
}

// ---- scratchpads --------------------------------------------------------------

pub fn scratchpad_list(state: &ControlState, _: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        state.coordination.scratchpad_list(&ScratchpadListQuery {
            project_id: a.pad_scope()?,
            include_global: a.bool("include_global")?.unwrap_or(false),
            query: a.str("query").map(str::to_owned),
            tags: a.strings("tags")?,
            include_archived: a.bool("include_archived")?.unwrap_or(false),
            offset: a.usize("offset")?.unwrap_or(0),
            limit: a.usize("limit")?,
        })
    })
}

pub fn scratchpad_tags_list(state: &ControlState, _: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        Ok(json!({"tags": state.coordination.scratchpad_tags(a.pad_scope()?)?}))
    })
}

/// `scratchpad_write` create half (`POST /scratchpads`).
pub fn scratchpad_create(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let name = a.req_str("name")?;
        let content = a.str("content").unwrap_or("").to_owned();
        let pad = state.coordination.scratchpad_create(
            &params.actor,
            a.project_scope()?,
            &name,
            &content,
            a.strings("tags")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("name", json!(pad.name)), ("created", json!(true))]))
    })
}

/// `scratchpad_write` overwrite half (`POST /scratchpads/{rid}`).
pub fn scratchpad_overwrite(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let id = record(params)?;
        let name = a.req_str("name")?;
        let content = a.str("content").unwrap_or("").to_owned();
        let tags = if a.get("tags").is_some() { Some(a.strings("tags")?) } else { None };
        let pad = state.coordination.scratchpad_overwrite(
            &params.actor,
            id,
            &name,
            &content,
            tags,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("name", json!(pad.name)), ("line_count", line_count(&pad))]))
    })
}

pub fn scratchpad_read(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let id = record(params)?;
        let heading = a.str("section_heading").or_else(|| a.str("heading"));
        let mode = ReadMode::parse(a.str("mode"), heading)?;
        state
            .coordination
            .scratchpad_read(id, &mode, a.usize("offset")?.unwrap_or(0), a.usize("limit")?)
    })
}

pub fn scratchpad_find(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let id = record(params)?;
        state.coordination.scratchpad_find(
            id,
            &FindQuery {
                query: a.req_str("query")?,
                case_sensitive: a.bool("case_sensitive")?.unwrap_or(false),
                limit: a.usize("limit")?,
                context_lines: a.usize("context_lines")?,
                scope: a.str("scope").map(str::to_owned),
            },
        )
    })
}

pub fn scratchpad_tail(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| state.coordination.scratchpad_tail(record(params)?, a.usize("lines")?))
}

pub fn scratchpad_rename(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let pad = state.coordination.scratchpad_rename(
            &params.actor,
            record(params)?,
            &a.req_str("name")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("name", json!(pad.name))]))
    })
}

pub fn scratchpad_append(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let pad = state.coordination.scratchpad_append(
            &params.actor,
            record(params)?,
            &a.req_str("content")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("line_count", line_count(&pad))]))
    })
}

pub fn scratchpad_append_section(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let heading = a.req_str("heading")?;
        let pad = state.coordination.scratchpad_append_section(
            &params.actor,
            record(params)?,
            &heading,
            &a.req_str("content")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("heading", json!(heading.trim())), ("line_count", line_count(&pad))]))
    })
}

pub fn scratchpad_edit(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let target = a
            .get("target")
            .ok_or_else(|| CoordError::Invalid("`target` is required".into()))?;
        let target = EditTarget::parse(target)?;
        let pad = state.coordination.scratchpad_edit(
            &params.actor,
            record(params)?,
            &target,
            &a.req_str("content")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("name", json!(pad.name)), ("line_count", line_count(&pad))]))
    })
}

pub fn scratchpad_add_tags(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let pad = state.coordination.scratchpad_add_tags(
            &params.actor,
            record(params)?,
            a.strings("tags")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("tags", json!(pad.tags))]))
    })
}

pub fn scratchpad_remove_tags(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let pad = state.coordination.scratchpad_remove_tags(
            &params.actor,
            record(params)?,
            a.strings("tags")?,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("tags", json!(pad.tags))]))
    })
}

pub fn scratchpad_clear(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        a.confirm("scratchpad_clear")?;
        let pad = state
            .coordination
            .scratchpad_clear(&params.actor, record(params)?, a.i64("expected_revision")?)?;
        Ok(pad_receipt(&pad, a.mode()?, &[("cleared", json!(true))]))
    })
}

pub fn scratchpad_delete(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        a.confirm("scratchpad_delete")?;
        let pad = state
            .coordination
            .scratchpad_delete(&params.actor, record(params)?, a.i64("expected_revision")?)?;
        Ok(json!({"scratchpad_id": pad.scratchpad_id, "project_id": pad.project_id, "deleted": true}))
    })
}

pub fn scratchpad_archive(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let archived = a.bool("archived")?.unwrap_or(true);
        let pad = state
            .coordination
            .scratchpad_archive(&params.actor, record(params)?, archived)?;
        Ok(pad_receipt(&pad, a.mode()?, &[("archived", json!(pad.archived))]))
    })
}

pub fn scratchpad_transfer(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        if a.0.get("target_project_id").is_none() {
            return Err(CoordError::Invalid("`target_project_id` is required (null = global)".into()));
        }
        let target = a.i64("target_project_id")?;
        let pad = state.coordination.scratchpad_transfer(
            &params.actor,
            record(params)?,
            target,
            a.i64("expected_revision")?,
        )?;
        Ok(pad_receipt(&pad, a.mode()?, &[("project_id", json!(pad.project_id))]))
    })
}

// ---- todos --------------------------------------------------------------------

pub fn todo_list(state: &ControlState, _: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        state.coordination.todo_list(&TodoListQuery {
            project_id: a.project_scope()?,
            status: a.str("status").map(str::to_owned),
            completed: a.bool("completed")?,
            is_blocked: a.bool("is_blocked")?,
            priority: a.str("priority").map(str::to_owned),
            query: a.str("query").map(str::to_owned),
            tags: a.strings("tags")?,
            sort: a.str("sort").map(str::to_owned),
            offset: a.usize("offset")?.unwrap_or(0),
            limit: a.usize("limit")?,
        })
    })
}

pub fn todo_tags_list(state: &ControlState, _: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| Ok(json!({"tags": state.coordination.todo_tags(a.project_scope()?)?})))
}

pub fn todo_create(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let project_id = a.project_scope()?.ok_or_else(|| {
            CoordError::Invalid("`project_id` is required (todos are project-scoped; chappa-ai-mcp fills it from CHAPPA_AI_PROJECT_ID)".into())
        })?;
        let todo = state.coordination.todo_create(
            &params.actor,
            project_id,
            &a.req_str("title")?,
            a.str("body").unwrap_or(""),
            a.str("priority"),
            a.strings("tags")?,
        )?;
        Ok(todo_receipt(&todo, a.mode()?, &[("title", json!(todo.title))]))
    })
}

pub fn todo_get(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let id = record(params)?;
        let todo = state.coordination.todo_get(id)?;
        let mut v = serde_json::to_value(&todo).unwrap_or_default();
        if a.bool("include_comments")?.unwrap_or(false) {
            v["comments"] = state.coordination.todo_comments(id, 0, Some(crate::coordination::MAX_LIST_LIMIT))?["comments"].clone();
        }
        Ok(v)
    })
}

pub fn todo_update(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let patch = TodoPatch {
            title: a.str("title").map(str::to_owned),
            body: a.str("body").map(str::to_owned),
            priority: a.str("priority").map(str::to_owned),
            status: a.str("status").map(str::to_owned),
            tags: if a.get("tags").is_some() { Some(a.strings("tags")?) } else { None },
            expected_revision: a.i64("expected_revision")?,
        };
        let (todo, changed) = state.coordination.todo_update(&params.actor, record(params)?, &patch)?;
        let row = serde_json::to_value(&todo).unwrap_or_default();
        let fields: Vec<(&str, Value)> = changed.iter().map(|k| (*k, row[*k].clone())).collect();
        Ok(todo_receipt(&todo, a.mode()?, &fields))
    })
}

pub fn todo_add_tag(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let tag = a.req_str("tag")?;
        let todo = state.coordination.todo_add_tag(&params.actor, record(params)?, &tag)?;
        Ok(todo_receipt(&todo, a.mode()?, &[("tag", json!(tag.trim())), ("tags", json!(todo.tags))]))
    })
}

pub fn todo_remove_tag(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let tag = a.req_str("tag")?;
        let todo = state.coordination.todo_remove_tag(&params.actor, record(params)?, &tag)?;
        Ok(todo_receipt(&todo, a.mode()?, &[("tag", json!(tag.trim())), ("tags", json!(todo.tags))]))
    })
}

fn blockers_reply(todo: &Todo, mode: ResponseMode) -> Value {
    todo_receipt(
        todo,
        mode,
        &[("blocker_ids", json!(todo.blocker_ids)), ("is_blocked", json!(todo.is_blocked))],
    )
}

pub fn todo_set_blockers(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let todo = state
            .coordination
            .todo_set_blockers(&params.actor, record(params)?, &a.ids("blocker_ids")?)?;
        Ok(blockers_reply(&todo, a.mode()?))
    })
}

pub fn todo_add_blocker(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let todo = state
            .coordination
            .todo_add_blocker(&params.actor, record(params)?, a.req_i64("blocker_id")?)?;
        Ok(blockers_reply(&todo, a.mode()?))
    })
}

pub fn todo_remove_blocker(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let todo = state
            .coordination
            .todo_remove_blocker(&params.actor, record(params)?, a.req_i64("blocker_id")?)?;
        Ok(blockers_reply(&todo, a.mode()?))
    })
}

pub fn todo_complete(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let completed = a
            .bool("completed")?
            .ok_or_else(|| CoordError::Invalid("`completed` is required".into()))?;
        let release = a.bool("release_lock")?.unwrap_or(true);
        let (todo, affected) = state
            .coordination
            .todo_complete(&params.actor, record(params)?, completed, release)?;
        Ok(todo_receipt(
            &todo,
            a.mode()?,
            &[
                ("completed", json!(todo.completed)),
                ("status", json!(todo.status)),
                ("locked_by", json!(todo.locked_by)),
                ("affected_todo_ids", json!(affected)),
            ],
        ))
    })
}

pub fn todo_lock(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        // `lease_ms` (preferred) or `lease_ttl_seconds`; the seconds
        // form saturates and is clamped to MAX_LEASE_MS (the store clamps
        // again).
        let lease = match a.i64("lease_ms")? {
            Some(ms) => Some(ms.max(0) as u64),
            None => a
                .i64("lease_ttl_seconds")?
                .map(|s| (s.max(0) as u64).saturating_mul(1000).min(crate::coordination::MAX_LEASE_MS)),
        };
        let todo = state.coordination.todo_lock(&params.actor, record(params)?, lease)?;
        Ok(todo_receipt(
            &todo,
            a.mode()?,
            &[("locked_by", json!(todo.locked_by)), ("lock_expires_at", json!(todo.lock_expires_at))],
        ))
    })
}

pub fn todo_unlock(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let todo = state.coordination.todo_unlock(&params.actor, record(params)?)?;
        Ok(todo_receipt(&todo, a.mode()?, &[("locked_by", json!(todo.locked_by))]))
    })
}

pub fn todo_transfer(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let target = a.req_i64("target_project_id")?;
        let (todo, affected) = state
            .coordination
            .todo_transfer(&params.actor, record(params)?, target)?;
        Ok(todo_receipt(
            &todo,
            a.mode()?,
            &[
                ("target_project_id", json!(target)),
                ("affected_todo_ids", json!(affected)),
                ("blocker_ids", json!(todo.blocker_ids)),
                ("locked_by", json!(todo.locked_by)),
            ],
        ))
    })
}

pub fn todo_delete(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        a.confirm("todo_delete")?;
        let (todo, affected) = state.coordination.todo_delete(&params.actor, record(params)?)?;
        Ok(json!({"todo_id": todo.todo_id, "project_id": todo.project_id, "deleted": true, "affected_todo_ids": affected}))
    })
}

pub fn todo_comment_list(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        state
            .coordination
            .todo_comments(record(params)?, a.usize("offset")?.unwrap_or(0), a.usize("limit")?)
    })
}

pub fn todo_comment_create(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let c = state
            .coordination
            .todo_comment_create(&params.actor, record(params)?, &a.req_str("body")?)?;
        Ok(match a.mode()? {
            ResponseMode::Rich => serde_json::to_value(&c).unwrap_or_default(),
            ResponseMode::Slim => json!({"comment_id": c.comment_id, "todo_id": c.todo_id}),
        })
    })
}

pub fn todo_comment_update(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let c = state
            .coordination
            .todo_comment_update(&params.actor, record(params)?, &a.req_str("body")?)?;
        Ok(match a.mode()? {
            ResponseMode::Rich => serde_json::to_value(&c).unwrap_or_default(),
            ResponseMode::Slim => json!({"comment_id": c.comment_id, "todo_id": c.todo_id}),
        })
    })
}

pub fn todo_comment_delete(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        a.confirm("todo_comment_delete")?;
        let c = state.coordination.todo_comment_delete(&params.actor, record(params)?)?;
        Ok(json!({"comment_id": c.comment_id, "todo_id": c.todo_id, "deleted": true}))
    })
}
