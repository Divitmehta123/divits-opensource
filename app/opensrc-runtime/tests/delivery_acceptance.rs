//! Real filesystem, subprocess and HTTP checks with a deterministic model boundary.
use async_trait::async_trait;
use opensrc_core::{
    CanonicalModelRequest, EvidenceStatus, ExecutionMode, ModelEvent, ProviderAdapter,
    ProviderCapabilities, ProviderError, RunStatus,
};
use opensrc_runtime::{
    AgentControl, AgentLimits, ExecutionEngine, LocalAccessProfile, ProviderRouter, ToolExecutor,
    built_in_agent_definition,
};
use opensrc_store::Store;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

const GOOD_APP: &str = "module.exports = (a,b) => a+b;\n";
const TEST: &str = "const test = require('node:test'); const assert = require('node:assert/strict'); const add = require('./app.cjs'); test('addition',()=>assert.equal(add(2,3),5)); test('negative',()=>assert.equal(add(-2,1),-1));";
const SERVER: &str = "const http=require('node:http'); const fs=require('node:fs'); const server=http.createServer((req,res)=>{res.writeHead(200,{'Content-Type':'text/html'});res.end(fs.readFileSync('index.html'));}); server.listen(0,'127.0.0.1',()=>process.send?.({port:server.address().port}));";
const SMOKE: &str = "const {fork}=require('node:child_process');const assert=require('node:assert/strict');const child=fork('./server.cjs',[],{stdio:['ignore','ignore','inherit','ipc']});const timer=setTimeout(()=>{child.kill();process.exit(1)},10000);child.once('message',async ({port})=>{try{const response=await fetch(`http://127.0.0.1:${port}`);assert.equal(response.status,200);assert.match(await response.text(),/Calculator/);console.log('HTTP startup smoke passed')}catch(error){console.error(error);process.exitCode=1}finally{clearTimeout(timer);child.kill()}});";

struct DeliveryModel {
    directory: PathBuf,
    step: AtomicUsize,
}

fn text(value: impl Into<String>) -> ModelEvent {
    ModelEvent::TextDelta { text: value.into() }
}
fn tool(id: usize, name: &str, arguments: Value) -> ModelEvent {
    ModelEvent::ToolCall {
        id: format!("delivery-{id}"),
        name: name.into(),
        arguments,
    }
}

#[async_trait]
impl ProviderAdapter for DeliveryModel {
    fn id(&self) -> &'static str {
        "delivery-fixture"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_tool_calls: true,
            ..ProviderCapabilities::default()
        }
    }
    async fn execute(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<Vec<ModelEvent>, ProviderError> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let files = [
            "app.cjs",
            "app.test.cjs",
            "index.html",
            "server.cjs",
            "smoke.cjs",
            "README.md",
        ];
        let events = match step {
            0 => {
                assert!(request.structured_output_schema.is_none());
                assert!(request.system.contains("\"validation_steps\""));
                assert!(
                    !self.directory.exists(),
                    "planning must precede directory creation"
                );
                vec![text(json!({"delivery_directory": self.directory, "tasks": [
                    {"id":"build","description":"Create the calculator source and tests with fs.write, then read back all files.","role":"implementer","dependencies":[],"owned_paths":["."],"deliverables":{"files":files,"description":"Runnable project"},"validation_steps":[]},
                    {"id":"verify","description":"Run tests, repair failures, and verify HTTP startup.","role":"test-debugging-specialist","depends_on":["build"],"owned_paths":["."],"deliverables":["Passing automated and startup checks"],"validation_steps":["node --test app.test.cjs","node smoke.cjs"]},
                    {"id":"release","description":"Independently rerun the application tests and HTTP startup check.","role":"release-specialist","dependencies":["verify"],"owned_paths":["."],"deliverables":["Observed release evidence"],"validation_steps":["node --test app.test.cjs","node smoke.cjs"]}
                ]}).to_string())]
            }
            1 => {
                assert!(request.system.contains(&*self.directory.to_string_lossy()));
                files.iter().zip(["module.exports = (a,b) => a-b;", TEST, "<!doctype html><title>Calculator</title><h1>Calculator</h1>", SERVER, SMOKE, "Run: node server.cjs\nTests: node --test app.test.cjs\nStartup smoke: node smoke.cjs"])
                    .enumerate().map(|(index,(path,content))| tool(100+index,"fs.write",json!({"path":path,"content":content}))).collect()
            }
            2 => vec![tool(step, "fs.read_many", json!({"paths":files}))],
            3 => vec![text(
                "Source and tests written and read back; validation is delegated.",
            )],
            4 | 8 | 11 => vec![tool(
                step,
                "shell.test",
                json!({"command":"node --test app.test.cjs"}),
            )],
            // A premature completion must be rejected, not converted to fabricated passes.
            5 => vec![text("Everything passed.")],
            6 => {
                let messages = serde_json::to_string(&request.messages).unwrap();
                assert!(
                    messages.contains("repair and rerun failed"),
                    "missing repair gate: {}",
                    serde_json::to_string(request.messages.last().unwrap()).unwrap()
                );
                vec![tool(
                    step,
                    "fs.write",
                    json!({"path":"app.cjs","content":GOOD_APP}),
                )]
            }
            7 => vec![tool(step, "fs.read", json!({"path":"app.cjs"}))],
            9 | 12 => vec![tool(
                step,
                "shell.test",
                json!({"command":"node smoke.cjs"}),
            )],
            10 => vec![text(
                "Two Node tests passed after repair. HTTP startup smoke passed. Run node server.cjs.",
            )],
            13 => vec![text(
                "Independent release verification: Node tests and HTTP startup passed.",
            )],
            14 => vec![text(
                "Delivery verified: source, tests, and HTTP startup are in one folder.",
            )],
            _ => panic!(
                "unexpected model cycle {step}: {}",
                serde_json::to_string(&request.messages).unwrap()
            ),
        };
        Ok(events
            .into_iter()
            .chain([ModelEvent::Completed {
                response_id: Some(format!("delivery-{step}")),
            }])
            .collect())
    }
}

#[tokio::test]
async fn plans_then_delivers_repairs_tests_and_starts_a_real_app_without_approvals() {
    assert!(
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok(),
        "Node is required by the delivery acceptance fixture"
    );
    let root = std::env::temp_dir().join(format!("divit-delivery-{}", Uuid::new_v4()));
    let launch = root.join("launcher");
    let output = root.join("Desktop").join("calculator");
    std::fs::create_dir_all(&launch).unwrap();
    let store = Store::in_memory().unwrap();
    let conversation = store
        .create_conversation(launch.to_string_lossy(), None)
        .unwrap();
    let request = "Make a working calculator application and run all its checks.";
    let run = store
        .create_run(conversation.id, request, ExecutionMode::Agentic)
        .unwrap();
    let mut definition = built_in_agent_definition("generalist").unwrap();
    LocalAccessProfile::trusted_host().apply_to_definition(&mut definition);
    definition.preferred_provider = Some("delivery-fixture".into());
    definition.preferred_model = Some("fixture".into());
    AgentControl::new(store.clone(), AgentLimits::default())
        .create_root(run.id, &definition, request, launch.to_string_lossy())
        .unwrap();
    let model = Arc::new(DeliveryModel {
        directory: output.clone(),
        step: AtomicUsize::new(0),
    });
    let router = ProviderRouter::default();
    router.register(model.clone());
    let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
    engine
        .execute_run(run.id, "delivery-fixture", "fixture")
        .await
        .expect("complete real delivery");
    assert_eq!(model.step.load(Ordering::SeqCst), 15);
    assert_eq!(store.get_run(run.id).unwrap().status, RunStatus::Completed);
    assert!(store.list_approvals(false).unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(output.join("app.cjs")).unwrap(),
        GOOD_APP
    );
    assert!(!launch.join("app.cjs").exists());
    assert_eq!(
        store
            .conversation_delivery_workspace(conversation.id)
            .unwrap(),
        Some(output.to_string_lossy().into_owned())
    );
    let validator = store
        .list_agents(Some(run.id))
        .unwrap()
        .into_iter()
        .find(|agent| agent.role == "test-debugging-specialist")
        .unwrap();
    let completion = store.get_agent_completion(validator.id).unwrap().unwrap();
    assert_eq!(completion.tests.len(), 2);
    assert!(
        completion
            .tests
            .iter()
            .all(|test| test.status == EvidenceStatus::Passed
                && test.evidence.contains("exit_code=0"))
    );
    assert!(completion.unresolved.is_empty());
    let events = store.events_after(0, 10_000).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == "tool.completed" && event.payload["status"] == "failed")
    );
    let planned = events
        .iter()
        .position(|event| event.kind == "agent.plan_created")
        .unwrap();
    let directory = events
        .iter()
        .position(|event| event.kind == "workspace.delivery_selected")
        .unwrap();
    assert!(planned < directory);
    // This unique temporary fixture is the only tree removed by this test.
    std::fs::remove_dir_all(root).unwrap();
}
