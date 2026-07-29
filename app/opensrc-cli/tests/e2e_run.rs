use axum::Json;
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};
use std::process::Stdio;
use uuid::Uuid;

#[tokio::test]
async fn installed_binary_runs_from_an_unrelated_project_and_persists_chat() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let provider_server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/v1/chat/completions", post(fixture_completion)),
        )
        .await
        .expect("fixture provider");
    });
    let app_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve app port");
    let app_port = app_listener.local_addr().expect("app address").port();
    drop(app_listener);
    let app_server = format!("http://127.0.0.1:{app_port}");

    let fixture_root =
        std::env::temp_dir().join(format!("opensource-binary-e2e-{}", Uuid::new_v4()));
    let project = fixture_root.join("unrelated-project");
    let local_app_data = fixture_root.join("local-app-data");
    let state = local_app_data.join("opensource");
    std::fs::create_dir_all(&project).expect("project directory");
    std::fs::create_dir_all(&state).expect("state directory");
    std::fs::write(
        state.join("providers.json"),
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "id": "fixture",
                "protocol": "openai_compatible",
                "family": "custom",
                "base_url": format!("http://{address}/v1"),
                "api_key_env": "OPENSOURCE_E2E_KEY",
                "default_model": "fixture-model"
            }]
        }))
        .expect("provider config"),
    )
    .expect("write provider config");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_divit"))
        .arg("run")
        .arg("hello from outside the repository")
        .arg("--project-root")
        .arg(&project)
        .arg("--provider")
        .arg("fixture")
        .arg("--model")
        .arg("fixture-model")
        .arg("--mode")
        .arg("direct")
        .arg("--server")
        .arg(&app_server)
        .current_dir(&project)
        .env("LOCALAPPDATA", &local_app_data)
        .env("OPENSOURCE_E2E_KEY", "redacted-fixture-key")
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run binary");
    assert!(
        output.status.success(),
        "binary failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("binary e2e answer"),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let store = opensrc_store::Store::open(state.join("state.sqlite3")).expect("reopen state");
    let project = std::fs::canonicalize(&project)
        .expect("canonical project")
        .to_string_lossy()
        .into_owned();
    let conversations = store
        .list_conversations(Some(&project))
        .expect("persisted conversations");
    assert_eq!(conversations.len(), 1);
    let messages = store
        .list_messages(conversations[0].id)
        .expect("persisted messages");
    assert_eq!(messages.len(), 2);
    drop(messages);
    drop(conversations);
    drop(store);
    provider_server.abort();
    std::fs::remove_dir_all(fixture_root).expect("cleanup");
}

async fn fixture_completion(Json(body): Json<Value>) -> impl IntoResponse {
    assert_eq!(body["model"], "fixture-model");
    (
        [("content-type", "text/event-stream")],
        "data: {\"id\":\"fixture-response\",\"choices\":[{\"delta\":{\"content\":\"binary e2e \"}}]}\n\
         \ndata: {\"id\":\"fixture-response\",\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\
         \ndata: [DONE]\n\n",
    )
}
