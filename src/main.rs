use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};  
use warp::ws::{Message, WebSocket};
use warp::Filter;
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::Reply;
use std::convert::Infallible;
use std::env;
use dotenv::dotenv;
use std::time::Duration;

// Define a type alias for our user storage
type Users = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<Result<Message, warp::Error>>>>>;

// Define the structure for the connection request
#[derive(Deserialize, Debug)]
struct ConnectRequest {
    password: String,
}

// Define the structure for video state
#[derive(Serialize, Deserialize, Debug, Clone)]
struct VideoState {
    #[serde(rename = "type")]
    message_type: String,
    time: Option<f64>,
    playing: Option<bool>,
    url: Option<String>,
}

// Constants for our application
const MAX_USERS: usize = 2;

// Global video state
type GlobalVideoState = Arc<RwLock<Option<VideoState>>>;

#[tokio::main]
async fn main() {
    dotenv().ok(); // Load .env file

    let correct_password = env::var("CORRECT_PASSWORD")
        .expect("CORRECT_PASSWORD must be set in environment");

    // Initialize our user storage
    let users = Users::default();
    let users_for_cleanup = users.clone();
    let users_filter = warp::any().map(move || users.clone());

    // Initialize global video state
    let global_video_state = GlobalVideoState::default();
    let global_video_state_filter = warp::any().map(move || global_video_state.clone());

    // Get the allowed origin from environment variable or use default
    let allowed_origin = env::var("ALLOWED_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".to_string());

    // CORS configuration
    let cors = warp::cors()
        .allow_origin(allowed_origin.as_str())
        .allow_methods(vec!["GET", "POST"])
        .allow_headers(vec!["Content-Type"])
        .max_age(3600);

    // Define the route for connecting to the server
    let connect = warp::post()
        .and(warp::path("connect"))
        .and(warp::body::json())
        .and(users_filter.clone())
        .and(warp::any().map(move || correct_password.clone()))
        .and_then(handle_connect);

    // Define the WebSocket route
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(users_filter)
        .and(global_video_state_filter)
        .map(|ws: warp::ws::Ws, users, state| {
            ws.on_upgrade(move |socket| user_connected(socket, users, state))
        });

    // Combine routes and apply CORS
    let routes = connect.or(ws_route).with(cors);

    // Start the cleanup task
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            cleanup_disconnected_users(&users_for_cleanup).await;
        }
    });

    // Start the server
    println!("Starting server on 0.0.0.0:3030");
    println!("Allowed origin: {}", allowed_origin);
    warp::serve(routes).run(([0, 0, 0, 0], 3030)).await;
}

async fn handle_connect(request: ConnectRequest, users: Users, correct_password: String) -> Result<impl Reply, Infallible> {
    if request.password != correct_password {
        return Ok(warp::reply::with_status("Incorrect password", warp::http::StatusCode::UNAUTHORIZED));
    }
    
    let users_count = users.read().await.iter().filter(|(_, tx)| !tx.is_closed()).count();
    if users_count >= MAX_USERS {
        return Ok(warp::reply::with_status("Room is full", warp::http::StatusCode::FORBIDDEN));
    }
    
    Ok(warp::reply::with_status("Connected", warp::http::StatusCode::OK))
}

async fn user_connected(ws: WebSocket, users: Users, state: GlobalVideoState) {
    let (user_ws_tx, mut user_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::task::spawn(UnboundedReceiverStream::new(rx)
        .forward(user_ws_tx)
        .map(|result| {
            if let Err(e) = result {
                eprintln!("websocket send error: {}", e);
            }
        })
    );

    let user_id = generate_user_id();
    users.write().await.insert(user_id.clone(), tx.clone());

    println!("New user connected: {}", user_id);

    // Send current state to the new user
    if let Some(current_state) = state.read().await.clone() {
        let _ = tx.send(Ok(Message::text(serde_json::to_string(&current_state).unwrap())));
    }

    while let Some(result) = user_ws_rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", user_id, e);
                break;
            }
        };
        user_message(user_id.clone(), msg, &users, &state).await;
    }

    user_disconnected(user_id, &users, &state).await;
}

async fn user_message(user_id: String, msg: Message, users: &Users, state: &GlobalVideoState) {
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    let video_state: VideoState = match serde_json::from_str(msg) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("Error parsing message from {}: {}", user_id, e);
            return;
        }
    };

    println!("Received message from {}: {:?}", user_id, video_state);

    match video_state.message_type.as_str() {
        "get_current_state" => {
            if let Some(current_state) = state.read().await.clone() {
                if let Some(tx) = users.read().await.get(&user_id) {
                    let _ = tx.send(Ok(Message::text(serde_json::to_string(&current_state).unwrap())));
                }
            }
            return;
        }
        "heartbeat" => {
            // Respond to heartbeat
            if let Some(tx) = users.read().await.get(&user_id) {
                let _ = tx.send(Ok(Message::text(serde_json::to_string(&VideoState {
                    message_type: "heartbeat_ack".to_string(),
                    time: None,
                    playing: None,
                    url: None,
                }).unwrap())));
            }
            return;
        }
        _ => {
            // Update global state for other message types
            *state.write().await = Some(video_state.clone());
        }
    }

    let new_msg = serde_json::to_string(&video_state).unwrap();

    for (uid, tx) in users.read().await.iter() {
        if *uid != user_id {
            if let Err(_disconnected) = tx.send(Ok(Message::text(new_msg.clone()))) {
                // The tx is disconnected, our `user_disconnected` code
                // should be handling this in another task
            }
        }
    }
}

async fn user_disconnected(user_id: String, users: &Users, state: &GlobalVideoState) {
    users.write().await.remove(&user_id);
    
    // Only clear the global state if there are no more connected users
    if users.read().await.is_empty() {
        *state.write().await = None;
    }
    
    println!("User disconnected: {}", user_id);
}

async fn cleanup_disconnected_users(users: &Users) {
    let mut users = users.write().await;
    users.retain(|_, tx| !tx.is_closed());
}

fn generate_user_id() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(7)
        .map(char::from)
        .collect()
}