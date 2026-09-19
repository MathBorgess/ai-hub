#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "wss://aihub.mathai.com.br".into());
    let id_path = aihub::identity::default_identity_path();
    let identity = aihub::identity::Identity::load_or_create(&id_path).expect("id");
    println!("pairing_code={}", identity.pairing_code());
    println!("connecting {url}");
    match aihub::remote::connect_with_response(&url, std::time::Duration::from_secs(10)).await {
        Err(e) => { println!("connect ERR class={:?} msg={}", e.class, e.message); return; }
        Ok((sock, resp)) => {
            println!("connect OK status={:?} date={:?}", resp.status(), resp.headers().get("date"));
            let mut io = aihub::remote::spawn_io(sock);
            match aihub::remote::perform_remote_handshake(&mut io, &identity, std::time::Duration::from_secs(10)).await {
                Ok(()) => println!("handshake OK"),
                Err(e) => println!("handshake ERR class={:?} msg={}", e.class, e.message),
            }
        }
    }
}
