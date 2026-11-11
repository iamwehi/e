//! The loop: request, stream, run tool calls in order, repeat until the
//! model replies without tool calls.

use crate::config::{Config, Meter};
use crate::exec::Cancel;
use crate::provider::{Client, Delta, End, FunctionCall, Message, ToolCall};
use crate::skills::{self, Skill};
use crate::transcript::{Transcript, now_unix};
use crate::{prompt, tools};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

pub enum Event<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    /// The assistant message is complete.
    TurnEnd,
    ToolStart {
        name: &'a str,
        args: &'a str,
        cost: String,
    },
    ToolEnd {
        summary: &'a str,
        content: &'a str,
        is_error: bool,
    },
    Notice(&'a str),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunEnd {
    /// Final assistant text.
    Done(String),
    Interrupted,
    Budget(String),
    Error(String),
}

impl RunEnd {
    pub fn exit_code(&self) -> u8 {
        match self {
            RunEnd::Done(_) => 0,
            RunEnd::Error(_) => 1,
            RunEnd::Budget(_) => 2,
            RunEnd::Interrupted => 130,
        }
    }
}

pub const WARN_PERCENT: u64 = 85;

pub struct Session {
    pub cfg: Config,
    pub model: String,
    pub cwd: PathBuf,
    /// Built once per session and never changed, so prefix caching can hit.
    pub system: String,
    pub history: Vec<Message>,
    pub meter: Meter,
    pub transcript: Transcript,
    pub warnings: Vec<String>,
    /// Every skill found, including those only the user may start.
    pub skills: Vec<Skill>,
    /// Skills whose instructions are already in history.
    activated: HashSet<String>,
    /// Context size after the latest response (prompt + completion tokens).
    pub context_tokens: u64,
    client: Client,
    tools: Value,
    seq: u32,
}

impl Session {
    pub fn new(cfg: Config, model: String, cwd: PathBuf, echo_json: bool) -> Result<Self, String> {
        let key = cfg.api_key().ok_or_else(|| format!("{} is not set", cfg.api_key_env))?;
        let client = Client::new(&cfg.base_url, key)?;
        let mut warnings = Vec::new();
        let skills = skills::discover(&skills::roots(&cwd), &mut warnings);
        let (system, prompt_warnings) = prompt::build(&cwd, &crate::transcript::today(), &skills);
        warnings.extend(prompt_warnings);
        let tools = tools::schemas();
        let mut transcript =
            Transcript::create(&cwd, echo_json).map_err(|e| format!("cannot create .e/runs: {e}"))?;
        transcript.log(
            "session",
            json!({"model": model, "cwd": cwd, "base_url": cfg.base_url, "system": system,
                   "tools": tools, "warnings": warnings}),
        );
        Ok(Session {
            cfg,
            model,
            cwd,
            system,
            history: Vec::new(),
            meter: Meter::default(),
            transcript,
            warnings,
            skills,
            activated: HashSet::new(),
            context_tokens: 0,
            client,
            tools,
            seq: 0,
        })
    }

    /// The user message for `/name task`: the skill's instructions, then the task. Once a skill
    /// is in history, a later `/name` refers back to it instead of repeating it. `None` if no
    /// skill has that name.
    pub fn skill_message(&mut self, name: &str, task: &str) -> Option<Result<String, String>> {
        let skill = self.skills.iter().find(|s| s.name == name)?;
        let loaded = if self.activated.contains(name) {
            format!("(The {name} skill's instructions are above.)")
        } else {
            match skills::activate(skill) {
                Ok(content) => {
                    self.activated.insert(name.to_string());
                    content
                }
                Err(e) => return Some(Err(e)),
            }
        };
        Some(Ok(match task.trim() {
            "" => format!("{loaded}\n\nThe user started the {name} skill."),
            task => format!("{loaded}\n\n{task}"),
        }))
    }

    fn body(&self) -> Value {
        let mut messages = Vec::with_capacity(self.history.len() + 1);
        messages.push(json!(Message::System { content: self.system.clone() }));
        messages.extend(self.history.iter().map(|m| json!(m)));
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": self.tools,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if let Some(n) = self.cfg.max_tokens {
            body["max_tokens"] = json!(n);
        }
        if let Some(e) = &self.cfg.reasoning_effort {
            body["reasoning_effort"] = json!(e);
        }
        if self.cfg.prompt_cache_key {
            // Constant for the session, so the request stays a stable prefix.
            let id = self.transcript.dir.file_name().map(|n| n.to_string_lossy().into_owned());
            body["prompt_cache_key"] = json!(format!("e-{}", id.unwrap_or_default()));
        }
        if let Some(r) = self.cfg.cache_retention(&self.model) {
            body["prompt_cache_retention"] = json!(r);
        }
        for (k, v) in &self.cfg.extra_body {
            body[k] = v.clone();
        }
        body
    }

    fn push_user(&mut self, content: String, kind: &str) {
        self.transcript.log(kind, json!({"content": content}));
        self.history.push(Message::User { content });
    }

    /// Run one task to completion. Every tool call in history gets a result
    /// before this returns, whatever the outcome.
    pub async fn run(&mut self, task: &str, cancel: &Cancel, ev: &mut dyn FnMut(Event)) -> RunEnd {
        self.push_user(task.to_string(), "user");
        let budget = self.cfg.context_budget.max(1);
        let mut warned = false;
        let end = loop {
            if cancel.is_cancelled() {
                break RunEnd::Interrupted;
            }
            let body = self.body();
            self.transcript.log("request", json!({"model": self.model, "messages": self.history.len() + 1}));
            let started = Instant::now();
            let (reply, end) = self
                .client
                .stream(&body, cancel, &mut |d| match d {
                    Delta::Text(t) => ev(Event::Text(t)),
                    Delta::Reasoning(t) => ev(Event::Reasoning(t)),
                    Delta::Notice(n) => ev(Event::Notice(&n)),
                })
                .await;
            ev(Event::TurnEnd);
            let usage = reply.usage.unwrap_or_default();
            let cost = reply.usage.and_then(|u| self.cfg.cost(&self.model, &u, now_unix()));
            self.meter.add(&usage, cost);
            if reply.usage.is_some() {
                self.context_tokens = usage.total();
            }
            self.transcript.log(
                "response",
                json!({"model": self.model, "end": format!("{end:?}"), "content": reply.content,
                       "reasoning_content": reply.reasoning, "tool_calls": reply.tool_calls,
                       "finish_reason": reply.finish_reason, "usage": reply.usage, "cost": cost,
                       "duration_ms": started.elapsed().as_millis() as u64}),
            );
            match end {
                End::Done => {}
                End::Cancelled => {
                    // Keep what was said; drop tool calls whose arguments may be cut off.
                    if !reply.content.is_empty() || reply.reasoning.is_some() {
                        self.history.push(Message::Assistant {
                            content: reply.content + "\n[interrupted by the user]",
                            reasoning_content: reply.reasoning,
                            tool_calls: Vec::new(),
                        });
                    }
                    break RunEnd::Interrupted;
                }
                End::Failed(e) => break RunEnd::Error(e),
            }
            let calls: Vec<ToolCall> = reply
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(i, mut c)| {
                    if c.id.is_empty() {
                        c.id = format!("call_e_{}_{i}", self.seq);
                    }
                    c
                })
                .collect();
            self.history.push(Message::Assistant {
                content: reply.content.clone(),
                reasoning_content: reply.reasoning,
                tool_calls: calls.clone(),
            });
            let finish = reply.finish_reason.unwrap_or_default();
            if calls.is_empty() {
                break match finish.as_str() {
                    "stop" | "" => RunEnd::Done(reply.content),
                    "length" => RunEnd::Error("the reply hit the output token limit".into()),
                    other => RunEnd::Error(format!("the reply stopped with finish_reason {other:?}")),
                };
            }
            let exhausted = self.context_tokens >= budget;
            for call in &calls {
                self.seq += 1;
                let skip = if exhausted {
                    Some("Not executed: the context budget is exhausted.")
                } else if finish == "length" {
                    Some(
                        "Not executed: the reply hit the output token limit, so the arguments may be cut off. Issue the call again.",
                    )
                } else if cancel.is_cancelled() {
                    Some("Not executed: interrupted by the user.")
                } else {
                    None
                };
                self.transcript.log(
                    "tool_call",
                    json!({"id": call.id, "name": call.function.name,
                                                        "arguments": call.function.arguments}),
                );
                let started = Instant::now();
                let out = match skip {
                    Some(msg) => tools::Output::err(msg.into()),
                    None => {
                        let FunctionCall { name, arguments } = &call.function;
                        ev(Event::ToolStart { name, args: arguments, cost: self.meter.display() });
                        let ctx = tools::Ctx {
                            cwd: &self.cwd,
                            run_dir: &self.transcript.dir,
                            cfg: &self.cfg,
                            cancel,
                            seq: self.seq,
                        };
                        let out = tools::dispatch(name, arguments, &ctx).await;
                        ev(Event::ToolEnd {
                            summary: &out.summary,
                            content: &out.content,
                            is_error: out.is_error,
                        });
                        out
                    }
                };
                self.transcript.log(
                    "tool_result",
                    json!({"id": call.id, "name": call.function.name,
                    "content": out.content, "is_error": out.is_error, "summary": out.summary,
                    "duration_ms": started.elapsed().as_millis() as u64}),
                );
                self.history.push(Message::Tool { tool_call_id: call.id.clone(), content: out.content });
            }
            if exhausted {
                break RunEnd::Budget(format!(
                    "context budget exhausted: {} of {budget} tokens",
                    self.context_tokens
                ));
            }
            if cancel.is_cancelled() {
                break RunEnd::Interrupted;
            }
            if !warned && self.context_tokens * 100 >= budget * WARN_PERCENT {
                warned = true;
                let msg = format!(
                    "[harness] {} of {budget} context tokens used. Update .e/NOTES.md now: what you covered, which hypotheses you ruled out and why, and what is left. Then stop and reply with your summary.",
                    self.context_tokens
                );
                ev(Event::Notice(&msg));
                self.push_user(msg, "budget_warning");
            }
        };
        self.transcript.log(
            "run_end",
            json!({"end": end_json(&end), "cost": self.meter.cost,
                                              "context_tokens": self.context_tokens}),
        );
        end
    }
}

fn end_json(end: &RunEnd) -> Value {
    match end {
        RunEnd::Done(t) => json!({"status": "done", "message": t}),
        RunEnd::Interrupted => json!({"status": "interrupted"}),
        RunEnd::Budget(m) => json!({"status": "budget_exhausted", "error": m}),
        RunEnd::Error(m) => json!({"status": "error", "error": m}),
    }
}

/// Checks the shape the API requires: every assistant tool call is answered
/// by a tool message, in order, before any other message.
pub fn history_is_valid(history: &[Message]) -> Result<(), String> {
    let mut pending: Vec<&str> = Vec::new();
    for (i, m) in history.iter().enumerate() {
        match m {
            Message::Tool { tool_call_id, .. } => {
                if pending.first() != Some(&tool_call_id.as_str()) {
                    return Err(format!("message {i}: unexpected tool result {tool_call_id}"));
                }
                pending.remove(0);
            }
            _ if !pending.is_empty() => {
                return Err(format!("message {i}: unanswered tool calls {pending:?}"));
            }
            Message::Assistant { tool_calls, .. } => {
                pending = tool_calls.iter().map(|c| c.id.as_str()).collect()
            }
            _ => {}
        }
    }
    if pending.is_empty() { Ok(()) } else { Err(format!("unanswered tool calls at end: {pending:?}")) }
}
