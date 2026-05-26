//! Gnome MCP Bridge — Desktop GUI
//!
//! Native window that talks to a running `gfb serve --transport http` over
//! HTTP/JSON-RPC. Five panels:
//!  1. Connection — server URL + bearer token (auto-loaded from
//!     ~/.gfb/agents/<id>.token if present).
//!  2. Health — periodic /healthz check + tools/list count.
//!  3. Tools — pick a tool, fill arguments, send, view response.
//!  4. Audit — tail ~/.gfb/audit.log (read directly from disk; the HTTP
//!     transport doesn't expose it).
//!  5. Pending approvals — list ~/.gfb/pending and approve from the UI.
//!
//! All HTTP is done on a background worker thread; the UI sends typed
//! requests through an mpsc channel and consumes typed responses on the
//! main thread inside `update()`. egui rerenders at ~60fps so any state
//! change is visible immediately without manual repaint plumbing.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Worker thread protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Req {
    Healthz { url: String },
    ListTools { url: String, token: String },
    CallTool { url: String, token: String, tool: String, args: Value },
    ReloadAudit { path: PathBuf, limit: usize },
    ReloadPending { dir: PathBuf },
    Approve { op_id: String, ttl_secs: u64 },
}

#[derive(Debug)]
enum Resp {
    Healthz(Result<String, String>),
    Tools(Result<Vec<ToolInfo>, String>),
    CallResult { tool: String, result: Result<Value, String> },
    Audit(Result<Vec<AuditEntry>, String>),
    Pending(Result<Vec<PendingEntry>, String>),
    ApproveResult(Result<String, String>),
}

#[derive(Debug, Clone, Deserialize)]
struct ToolInfo {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct AuditEntry {
    timestamp: chrono::DateTime<chrono::Utc>,
    agent_id: String,
    operation: String,
    target_path: PathBuf,
    status: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    before_hash: Option<String>,
    #[serde(default)]
    after_hash: Option<String>,
    #[serde(default)]
    diff: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PendingEntry {
    op_id: String,
    agent_id: String,
    tool: String,
    summary: String,
    approved: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn spawn_worker(ctx: egui::Context) -> (Sender<Req>, Receiver<Resp>) {
    let (req_tx, req_rx) = channel::<Req>();
    let (resp_tx, resp_rx) = channel::<Resp>();
    thread::spawn(move || {
        while let Ok(req) = req_rx.recv() {
            let resp = handle(req);
            let _ = resp_tx.send(resp);
            ctx.request_repaint();
        }
    });
    (req_tx, resp_rx)
}

fn handle(req: Req) -> Resp {
    match req {
        Req::Healthz { url } => Resp::Healthz(do_healthz(&url)),
        Req::ListTools { url, token } => Resp::Tools(do_list_tools(&url, &token)),
        Req::CallTool { url, token, tool, args } => Resp::CallResult {
            tool: tool.clone(),
            result: do_call_tool(&url, &token, &tool, &args),
        },
        Req::ReloadAudit { path, limit } => Resp::Audit(do_read_audit(&path, limit)),
        Req::ReloadPending { dir } => Resp::Pending(do_read_pending(&dir)),
        Req::Approve { op_id, ttl_secs } => {
            Resp::ApproveResult(do_approve(&op_id, ttl_secs))
        }
    }
}

fn do_healthz(url: &str) -> Result<String, String> {
    let resp = ureq::get(&format!("{}/healthz", url.trim_end_matches('/')))
        .timeout(Duration::from_secs(2))
        .call()
        .map_err(|e| e.to_string())?;
    Ok(resp.into_string().unwrap_or_default())
}

fn do_list_tools(url: &str, token: &str) -> Result<Vec<ToolInfo>, String> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    let resp: Value = ureq::post(&format!("{}/rpc", url.trim_end_matches('/')))
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(5))
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    if let Some(err) = resp.get("error") {
        return Err(err.to_string());
    }
    let arr = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("unexpected response: {}", resp))?;
    let tools: Vec<ToolInfo> = arr
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    Ok(tools)
}

fn do_call_tool(url: &str, token: &str, tool: &str, args: &Value) -> Result<Value, String> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":2,
        "method":"tools/call",
        "params":{"name": tool, "arguments": args}
    });
    let resp: Value = ureq::post(&format!("{}/rpc", url.trim_end_matches('/')))
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(30))
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;
    if let Some(err) = resp.get("error") {
        return Err(serde_json::to_string_pretty(err).unwrap_or_default());
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

fn do_read_audit(path: &std::path::Path, limit: usize) -> Result<Vec<AuditEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut entries: Vec<AuditEntry> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    entries.truncate(limit);
    Ok(entries)
}

fn do_read_pending(dir: &std::path::Path) -> Result<Vec<PendingEntry>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let body = match std::fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(p) = serde_json::from_slice::<PendingEntry>(&body) {
            out.push(p);
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(p.created_at));
    Ok(out)
}

fn do_approve(op_id: &str, ttl_secs: u64) -> Result<String, String> {
    // Shell out to `gfb approve` so we use the official code path (and
    // pick up future changes to approval storage automatically).
    let out = std::process::Command::new("gfb")
        .args(["approve", "--operation-id", op_id])
        .env("GFB_APPROVE_TTL_SECS", ttl_secs.to_string())
        .output()
        .map_err(|e| format!("failed to run `gfb approve`: {}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

fn default_bearer_token() -> String {
    dirs::home_dir()
        .map(|h| h.join(".gfb/agents/kimi-desktop.token"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn default_audit_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".gfb/audit.log"))
        .unwrap_or_else(|| PathBuf::from(".gfb/audit.log"))
}

fn default_pending_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".gfb/pending"))
        .unwrap_or_else(|| PathBuf::from(".gfb/pending"))
}

struct App {
    url: String,
    token: String,
    health: String,
    last_health: Option<Instant>,
    tools: Vec<ToolInfo>,
    tools_err: Option<String>,
    selected_tool: Option<String>,
    args_json: String,
    call_status: Option<String>,
    call_response: Option<String>,
    audit_path: String,
    audit: Vec<AuditEntry>,
    audit_err: Option<String>,
    audit_limit: usize,
    pending_dir: String,
    pending: Vec<PendingEntry>,
    pending_err: Option<String>,
    approve_status: Option<String>,
    selected_audit: Option<usize>,
    req_tx: Sender<Req>,
    resp_rx: Receiver<Resp>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (req_tx, resp_rx) = spawn_worker(cc.egui_ctx.clone());
        let url = "http://127.0.0.1:18765".to_string();
        let token = default_bearer_token();
        let audit_path = default_audit_path().to_string_lossy().into_owned();
        let pending_dir = default_pending_dir().to_string_lossy().into_owned();

        let mut app = Self {
            url,
            token,
            health: String::from("(not checked yet)"),
            last_health: None,
            tools: Vec::new(),
            tools_err: None,
            selected_tool: None,
            args_json: r#"{"path":"/tmp/x.txt"}"#.into(),
            call_status: None,
            call_response: None,
            audit_path,
            audit: Vec::new(),
            audit_err: None,
            audit_limit: 100,
            pending_dir,
            pending: Vec::new(),
            pending_err: None,
            approve_status: None,
            selected_audit: None,
            req_tx,
            resp_rx,
        };
        // Kick off an initial healthz check and an audit-log read so the
        // user sees something other than empty panels on first paint.
        app.dispatch_healthz();
        app.dispatch_audit();
        app.dispatch_pending();
        app
    }

    fn dispatch_healthz(&self) {
        let _ = self.req_tx.send(Req::Healthz { url: self.url.clone() });
    }

    fn dispatch_list_tools(&self) {
        let _ = self.req_tx.send(Req::ListTools {
            url: self.url.clone(),
            token: self.token.clone(),
        });
    }

    fn dispatch_call(&self) {
        if let Some(name) = self.selected_tool.clone() {
            let args: Value = serde_json::from_str(&self.args_json).unwrap_or(Value::Object(Default::default()));
            let _ = self.req_tx.send(Req::CallTool {
                url: self.url.clone(),
                token: self.token.clone(),
                tool: name,
                args,
            });
        }
    }

    fn dispatch_audit(&self) {
        let _ = self.req_tx.send(Req::ReloadAudit {
            path: PathBuf::from(&self.audit_path),
            limit: self.audit_limit,
        });
    }

    fn dispatch_pending(&self) {
        let _ = self.req_tx.send(Req::ReloadPending {
            dir: PathBuf::from(&self.pending_dir),
        });
    }

    fn drain_responses(&mut self) {
        while let Ok(resp) = self.resp_rx.try_recv() {
            match resp {
                Resp::Healthz(r) => {
                    self.health = match r {
                        Ok(body) => format!("✓ {}", body.trim()),
                        Err(e) => format!("✗ {}", e),
                    };
                    self.last_health = Some(Instant::now());
                }
                Resp::Tools(r) => match r {
                    Ok(tools) => {
                        self.tools = tools;
                        self.tools_err = None;
                        if self.selected_tool.is_none() {
                            self.selected_tool = self.tools.first().map(|t| t.name.clone());
                        }
                    }
                    Err(e) => self.tools_err = Some(e),
                },
                Resp::CallResult { tool, result } => match result {
                    Ok(v) => {
                        self.call_status = Some(format!("ok: {}", tool));
                        self.call_response = Some(serde_json::to_string_pretty(&v).unwrap_or_default());
                    }
                    Err(e) => {
                        self.call_status = Some(format!("err: {}", tool));
                        self.call_response = Some(e);
                    }
                },
                Resp::Audit(r) => match r {
                    Ok(a) => {
                        self.audit = a;
                        self.audit_err = None;
                    }
                    Err(e) => self.audit_err = Some(e),
                },
                Resp::Pending(r) => match r {
                    Ok(p) => {
                        self.pending = p;
                        self.pending_err = None;
                    }
                    Err(e) => self.pending_err = Some(e),
                },
                Resp::ApproveResult(r) => {
                    self.approve_status = Some(match r {
                        Ok(s) => format!("✓ {}", s.trim()),
                        Err(e) => format!("✗ {}", e),
                    });
                    self.dispatch_pending();
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_responses();

        // Background poll: healthz every 5s, audit every 3s.
        if self.last_health.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) {
            self.dispatch_healthz();
        }
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🛡 Gnome MCP Bridge");
                ui.add_space(20.0);
                ui.label(format!("server: {}", self.health));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳ refresh all").clicked() {
                        self.dispatch_healthz();
                        self.dispatch_list_tools();
                        self.dispatch_audit();
                        self.dispatch_pending();
                    }
                });
            });
        });

        egui::SidePanel::left("nav").default_width(220.0).show(ctx, |ui| {
            ui.heading("Connection");
            ui.label("Server URL");
            ui.text_edit_singleline(&mut self.url);
            ui.label("Bearer token");
            ui.add(egui::TextEdit::singleline(&mut self.token).password(true));
            if ui.button("Test /healthz").clicked() {
                self.dispatch_healthz();
            }
            if ui.button("Load tools list").clicked() {
                self.dispatch_list_tools();
            }
            ui.separator();
            ui.heading("Audit");
            ui.label("Log path");
            ui.text_edit_singleline(&mut self.audit_path);
            ui.add(egui::Slider::new(&mut self.audit_limit, 10..=500).text("entries"));
            if ui.button("Reload audit").clicked() {
                self.dispatch_audit();
            }
            ui.separator();
            ui.heading("Approvals");
            ui.label("Pending dir");
            ui.text_edit_singleline(&mut self.pending_dir);
            if ui.button("Reload pending").clicked() {
                self.dispatch_pending();
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Audit log (newest first)");
            if let Some(err) = &self.audit_err {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("error: {}", err));
            }
            egui::ScrollArea::vertical().id_salt("audit").max_height(280.0).show(ui, |ui| {
                use egui_extras::{Column, TableBuilder};
                TableBuilder::new(ui)
                    .striped(true)
                    .column(Column::auto().at_least(160.0)) // timestamp
                    .column(Column::auto().at_least(80.0))  // agent
                    .column(Column::auto().at_least(80.0))  // op
                    .column(Column::auto().at_least(80.0))  // status
                    .column(Column::remainder().at_least(260.0)) // path
                    .header(20.0, |mut h| {
                        h.col(|ui| { ui.strong("when"); });
                        h.col(|ui| { ui.strong("agent"); });
                        h.col(|ui| { ui.strong("op"); });
                        h.col(|ui| { ui.strong("status"); });
                        h.col(|ui| { ui.strong("target"); });
                    })
                    .body(|mut body| {
                        for (i, e) in self.audit.iter().enumerate() {
                            body.row(18.0, |mut row| {
                                row.col(|ui| {
                                    let lbl = ui.selectable_label(self.selected_audit == Some(i), e.timestamp.format("%H:%M:%S").to_string());
                                    if lbl.clicked() {
                                        self.selected_audit = Some(i);
                                    }
                                });
                                row.col(|ui| { ui.label(&e.agent_id); });
                                row.col(|ui| { ui.label(&e.operation); });
                                row.col(|ui| {
                                    let col = match e.status.as_str() {
                                        "Success" => egui::Color32::LIGHT_GREEN,
                                        "Denied"  => egui::Color32::LIGHT_RED,
                                        "Pending" => egui::Color32::YELLOW,
                                        _         => egui::Color32::GRAY,
                                    };
                                    ui.colored_label(col, &e.status);
                                });
                                row.col(|ui| { ui.label(e.target_path.display().to_string()); });
                            });
                        }
                    });
            });

            if let Some(idx) = self.selected_audit {
                if let Some(e) = self.audit.get(idx) {
                    ui.separator();
                    ui.heading("Selected entry");
                    ui.label(format!("Time: {}  •  duration {}ms", e.timestamp, e.duration_ms));
                    if let (Some(b), Some(a)) = (&e.before_hash, &e.after_hash) {
                        ui.label(format!("before: {}  →  after: {}", &b[..b.len().min(16)], &a[..a.len().min(16)]));
                    }
                    if let Some(d) = &e.diff {
                        ui.label("Diff:");
                        egui::ScrollArea::vertical().id_salt("diff").max_height(140.0).show(ui, |ui| {
                            ui.add(egui::TextEdit::multiline(&mut d.clone()).code_editor().desired_width(f32::INFINITY));
                        });
                    }
                }
            }

            ui.separator();
            ui.collapsing("Pending approvals", |ui| {
                if let Some(err) = &self.pending_err {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("error: {}", err));
                }
                if let Some(status) = &self.approve_status {
                    ui.label(status);
                }
                if self.pending.is_empty() {
                    ui.label("(none)");
                }
                let mut to_approve: Option<String> = None;
                for p in &self.pending {
                    ui.horizontal(|ui| {
                        if p.approved {
                            ui.colored_label(egui::Color32::LIGHT_GREEN, "approved");
                        } else {
                            ui.colored_label(egui::Color32::YELLOW, "pending");
                        }
                        ui.label(format!("{} • {} → {}", &p.op_id[..p.op_id.len().min(8)], p.agent_id, p.tool));
                        ui.label(&p.summary);
                        if !p.approved && ui.button("Approve").clicked() {
                            to_approve = Some(p.op_id.clone());
                        }
                    });
                }
                if let Some(op) = to_approve {
                    let _ = self.req_tx.send(Req::Approve { op_id: op, ttl_secs: 300 });
                }
            });

            ui.separator();
            ui.collapsing("Tools palette", |ui| {
                if let Some(err) = &self.tools_err {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("error: {}", err));
                }
                ui.horizontal(|ui| {
                    egui::ComboBox::from_label("tool")
                        .selected_text(self.selected_tool.clone().unwrap_or_else(|| "(none loaded)".into()))
                        .show_ui(ui, |ui| {
                            for t in &self.tools {
                                ui.selectable_value(&mut self.selected_tool, Some(t.name.clone()), &t.name);
                            }
                        });
                    if ui.button("Send").clicked() {
                        self.dispatch_call();
                    }
                });
                if let Some(name) = &self.selected_tool {
                    if let Some(t) = self.tools.iter().find(|t| &t.name == name) {
                        ui.small(&t.description);
                        if ui.button("Copy schema → arguments").clicked() {
                            self.args_json = serde_json::to_string_pretty(
                                &t.input_schema.get("properties").cloned().unwrap_or(Value::Object(Default::default())),
                            )
                            .unwrap_or_default();
                        }
                    }
                }
                ui.label("Arguments (JSON):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.args_json)
                        .code_editor()
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                if let Some(status) = &self.call_status {
                    ui.label(status);
                }
                if let Some(resp) = &mut self.call_response {
                    ui.label("Response:");
                    egui::ScrollArea::vertical().id_salt("resp").max_height(200.0).show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(resp).code_editor().desired_width(f32::INFINITY));
                    });
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Gnome MCP Bridge"),
        ..Default::default()
    };
    eframe::run_native(
        "Gnome MCP Bridge",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
