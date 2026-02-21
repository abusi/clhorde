//! Standalone test client for the clhorded daemon.
//!
//! Usage:
//!   cargo run --example test_client --package clhorde-daemon
//!
//! Connects to the daemon socket, subscribes, queries state,
//! submits a test prompt, streams events, then shuts down.

use std::io::Write;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use clhorde_core::ipc::{daemon_socket_path, is_binary_frame, decode_pty_frame};
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

async fn write_frame(
    stream: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> Result<Vec<u8>, std::io::Error> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn send_request(
    stream: &mut (impl AsyncWriteExt + Unpin),
    req: &ClientRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_vec(req)?;
    write_frame(stream, &json).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = daemon_socket_path();
    println!("Connecting to {}", socket_path.display());

    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Subscribe to events
    println!(">>> Subscribe");
    send_request(&mut writer, &ClientRequest::Subscribe).await?;

    // Get current state
    println!(">>> GetState");
    send_request(&mut writer, &ClientRequest::GetState).await?;

    // Submit a test prompt
    println!(">>> SubmitPrompt (echo test)");
    send_request(
        &mut writer,
        &ClientRequest::SubmitPrompt {
            text: "Say hello world in one line".to_string(),
            cwd: None,
            mode: "one-shot".to_string(),
            worktree: false,
            tags: vec!["test".to_string()],
        },
    )
    .await?;

    // Read events until the prompt finishes or we timeout
    println!("\n--- Streaming events ---\n");

    let timeout = tokio::time::Duration::from_secs(60);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let payload = tokio::select! {
            result = read_frame(&mut reader) => {
                match result {
                    Ok(p) => p,
                    Err(e) => {
                        println!("Connection closed: {e}");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                println!("Timeout waiting for events");
                break;
            }
        };

        if is_binary_frame(&payload) {
            if let Ok((prompt_id, data)) = decode_pty_frame(&payload) {
                print!("[PTY #{prompt_id}] {} bytes: ", data.len());
                std::io::stdout().flush().ok();
                std::io::stdout().write_all(&data).ok();
                println!();
            }
            continue;
        }

        let event: DaemonEvent = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(e) => {
                println!("Failed to parse event: {e}");
                continue;
            }
        };

        match &event {
            DaemonEvent::StateSnapshot(state) => {
                println!(
                    "StateSnapshot: {} prompts, max_workers={}, active={}",
                    state.prompts.len(),
                    state.max_workers,
                    state.active_workers
                );
            }
            DaemonEvent::PromptAdded(info) => {
                println!("PromptAdded: #{} \"{}\"", info.id, info.text);
            }
            DaemonEvent::PromptUpdated(info) => {
                println!(
                    "PromptUpdated: #{} status={} output_len={}",
                    info.id, info.status, info.output_len
                );
            }
            DaemonEvent::OutputChunk { prompt_id, text } => {
                print!("[Output #{prompt_id}] {text}");
                std::io::stdout().flush().ok();
            }
            DaemonEvent::WorkerStarted { prompt_id } => {
                println!("WorkerStarted: #{prompt_id}");
            }
            DaemonEvent::WorkerFinished {
                prompt_id,
                exit_code,
            } => {
                println!("WorkerFinished: #{prompt_id} exit_code={exit_code:?}");
                // Prompt finished — send shutdown
                println!("\n>>> Shutdown");
                send_request(&mut writer, &ClientRequest::Shutdown).await?;
                break;
            }
            DaemonEvent::WorkerError { prompt_id, error } => {
                println!("WorkerError: #{prompt_id} {error}");
                println!("\n>>> Shutdown");
                send_request(&mut writer, &ClientRequest::Shutdown).await?;
                break;
            }
            DaemonEvent::ActiveWorkersChanged { count } => {
                println!("ActiveWorkersChanged: {count}");
            }
            DaemonEvent::Pong => {
                println!("Pong");
            }
            other => {
                println!("Event: {other:?}");
            }
        }
    }

    println!("\nTest client done.");
    Ok(())
}
