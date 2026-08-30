#[tokio::main]
async fn main() {
    if let Err(error) = voxtype_meeting_service::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
