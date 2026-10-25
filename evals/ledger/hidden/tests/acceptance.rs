//! Black-box acceptance tests for the ledger contract. Each test starts the
//! built binary on free ports and talks to it over HTTP and gRPC.

use std::net::TcpListener;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use ledger::pb::ledger_client::LedgerClient;
use serde_json::Value;
use serde_json::json;

struct Server {
    child: Child,
    http: String,
    grpc: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start() -> Server {
    let (http, grpc) = (free_port(), free_port());
    let child = Command::new(env!("CARGO_BIN_EXE_ledger"))
        .env("LEDGER_HTTP_ADDR", format!("127.0.0.1:{http}"))
        .env("LEDGER_GRPC_ADDR", format!("127.0.0.1:{grpc}"))
        .spawn()
        .expect("the ledger binary starts");
    let started = Instant::now();
    while [http, grpc]
        .iter()
        .any(|port| std::net::TcpStream::connect(("127.0.0.1", *port)).is_err())
    {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the servers did not start listening within 20 seconds"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Server {
        child,
        http: format!("http://127.0.0.1:{http}"),
        grpc: format!("http://127.0.0.1:{grpc}"),
    }
}

async fn send(method: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let client = reqwest::Client::new();
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("the request reaches the server");
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn post(server: &Server, path: &str, body: Value) -> (u16, Value) {
    send(reqwest::Method::POST, format!("{}{path}", server.http), Some(body)).await
}

async fn get(server: &Server, path: &str) -> (u16, Value) {
    send(reqwest::Method::GET, format!("{}{path}", server.http), None).await
}

async fn grpc(server: &Server) -> LedgerClient<tonic::transport::Channel> {
    LedgerClient::connect(server.grpc.clone())
        .await
        .expect("the gRPC client connects")
}

#[tokio::test]
async fn http_ops_create_deposit_withdraw_and_read() {
    let server = start();
    let (status, account) = post(&server, "/accounts", json!({"id": "alice"})).await;
    assert_eq!((status, account), (201, json!({"id": "alice", "balance": 0})));
    let (status, account) = post(&server, "/accounts/alice/deposit", json!({"amount": 500})).await;
    assert_eq!((status, account["balance"].clone()), (200, json!(500)));
    let (status, account) = post(&server, "/accounts/alice/withdraw", json!({"amount": 120})).await;
    assert_eq!((status, account["balance"].clone()), (200, json!(380)));
    let (status, account) = get(&server, "/accounts/alice").await;
    assert_eq!((status, account), (200, json!({"id": "alice", "balance": 380})));
}

#[tokio::test]
async fn http_ops_transfer_moves_money_between_accounts() {
    let server = start();
    post(&server, "/accounts", json!({"id": "a"})).await;
    post(&server, "/accounts", json!({"id": "b"})).await;
    post(&server, "/accounts/a/deposit", json!({"amount": 100})).await;
    let (status, reply) = post(&server, "/transfers", json!({"from": "a", "to": "b", "amount": 30})).await;
    assert_eq!(status, 200);
    assert_eq!(reply["from"], json!({"id": "a", "balance": 70}));
    assert_eq!(reply["to"], json!({"id": "b", "balance": 30}));
}

#[tokio::test]
async fn grpc_ops_create_deposit_withdraw_and_read() {
    use ledger::pb::{AmountRequest, CreateAccountRequest, GetBalanceRequest};
    let server = start();
    let mut client = grpc(&server).await;
    let account = client
        .create_account(CreateAccountRequest { id: "carol".into() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((account.id.as_str(), account.balance), ("carol", 0));
    let account = client
        .deposit(AmountRequest { id: "carol".into(), amount: 900 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(account.balance, 900);
    let account = client
        .withdraw(AmountRequest { id: "carol".into(), amount: 400 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(account.balance, 500);
    let account = client
        .get_balance(GetBalanceRequest { id: "carol".into() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(account.balance, 500);
}

#[tokio::test]
async fn grpc_ops_transfer_and_both_apis_share_one_store() {
    use ledger::pb::{CreateAccountRequest, TransferRequest};
    let server = start();
    let mut client = grpc(&server).await;
    client.create_account(CreateAccountRequest { id: "x".into() }).await.unwrap();
    post(&server, "/accounts", json!({"id": "y"})).await;
    post(&server, "/accounts/x/deposit", json!({"amount": 50})).await;
    let reply = client
        .transfer(TransferRequest { from: "x".into(), to: "y".into(), amount: 20 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reply.from.unwrap().balance, 30);
    assert_eq!(reply.to.unwrap().balance, 20);
    let (_, account) = get(&server, "/accounts/y").await;
    assert_eq!(account["balance"], json!(20));
}

#[tokio::test]
async fn errors_over_http_have_the_contract_statuses() {
    let server = start();
    post(&server, "/accounts", json!({"id": "a"})).await;
    post(&server, "/accounts", json!({"id": "b"})).await;
    post(&server, "/accounts/a/deposit", json!({"amount": 10})).await;
    let cases = [
        (post(&server, "/accounts", json!({"id": "a"})).await, 409),
        (get(&server, "/accounts/nobody").await, 404),
        (post(&server, "/accounts/nobody/deposit", json!({"amount": 5})).await, 404),
        (post(&server, "/accounts/a/deposit", json!({"amount": 0})).await, 400),
        (post(&server, "/accounts/a/deposit", json!({"amount": -5})).await, 400),
        (post(&server, "/accounts/a/withdraw", json!({"amount": 11})).await, 422),
        (post(&server, "/transfers", json!({"from": "a", "to": "b", "amount": 11})).await, 422),
        (post(&server, "/transfers", json!({"from": "a", "to": "a", "amount": 1})).await, 400),
    ];
    for (index, ((status, body), expected)) in cases.into_iter().enumerate() {
        assert_eq!(status, expected, "case {index}: {body}");
        assert!(body["error"].is_string(), "case {index}: no error message in {body}");
    }
    let (_, account) = get(&server, "/accounts/a").await;
    assert_eq!(account["balance"], json!(10), "a failed operation changed the balance");
}

#[tokio::test]
async fn errors_over_grpc_have_the_contract_codes() {
    use ledger::pb::{AmountRequest, CreateAccountRequest, GetBalanceRequest, TransferRequest};
    use tonic::Code;
    let server = start();
    let mut client = grpc(&server).await;
    client.create_account(CreateAccountRequest { id: "a".into() }).await.unwrap();
    client.create_account(CreateAccountRequest { id: "b".into() }).await.unwrap();
    client.deposit(AmountRequest { id: "a".into(), amount: 10 }).await.unwrap();

    let code = |result: Result<tonic::Response<ledger::pb::Account>, tonic::Status>| {
        result.err().map(|status| status.code())
    };
    assert_eq!(code(client.create_account(CreateAccountRequest { id: "a".into() }).await), Some(Code::AlreadyExists));
    assert_eq!(code(client.get_balance(GetBalanceRequest { id: "zed".into() }).await), Some(Code::NotFound));
    assert_eq!(code(client.deposit(AmountRequest { id: "a".into(), amount: 0 }).await), Some(Code::InvalidArgument));
    assert_eq!(code(client.withdraw(AmountRequest { id: "a".into(), amount: 11 }).await), Some(Code::FailedPrecondition));
    let same = client
        .transfer(TransferRequest { from: "a".into(), to: "a".into(), amount: 1 })
        .await
        .err()
        .map(|status| status.code());
    assert_eq!(same, Some(Code::InvalidArgument));
    let over = client
        .transfer(TransferRequest { from: "a".into(), to: "b".into(), amount: 11 })
        .await
        .err()
        .map(|status| status.code());
    assert_eq!(over, Some(Code::FailedPrecondition));
}

#[tokio::test]
async fn concurrent_transfers_conserve_money_and_never_overdraw() {
    let server = start();
    post(&server, "/accounts", json!({"id": "a"})).await;
    post(&server, "/accounts", json!({"id": "b"})).await;
    post(&server, "/accounts/a/deposit", json!({"amount": 50})).await;

    let mut tasks = Vec::new();
    for _ in 0..20 {
        let url = format!("{}/transfers", server.http);
        tasks.push(tokio::spawn(async move {
            send(reqwest::Method::POST, url, Some(json!({"from": "a", "to": "b", "amount": 10}))).await.0
        }));
    }
    let mut succeeded = 0;
    for task in tasks {
        let status = task.await.unwrap();
        assert!(status == 200 || status == 422, "unexpected status {status}");
        if status == 200 {
            succeeded += 1;
        }
    }
    let (_, a) = get(&server, "/accounts/a").await;
    let (_, b) = get(&server, "/accounts/b").await;
    assert_eq!(succeeded, 5, "exactly five transfers of 10 fit in 50");
    assert_eq!((a["balance"].clone(), b["balance"].clone()), (json!(0), json!(50)));
}

#[test]
fn shutdown_on_sigint_exits_zero_within_five_seconds() {
    let mut server = start();
    let pid = server.child.id().to_string();
    let signalled = Command::new("kill").args(["-INT", &pid]).status().unwrap();
    assert!(signalled.success(), "could not signal the server");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the server was still running five seconds after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "the server exited with {status}");
}
