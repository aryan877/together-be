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
    let users = warp::any().map(move || users.clone());

    // Initialize global video state
    let global_video_state = GlobalVideoState::default();
    let global_video_state = warp::any().map(move || global_video_state.clone());

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
        .and(users.clone())
        .and(warp::any().map(move || correct_password.clone()))
        .and_then(handle_connect);

    // Define the WebSocket route
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(users)
        .and(global_video_state)
        .map(|ws: warp::ws::Ws, users, state| {
            ws.on_upgrade(move |socket| user_connected(socket, users, state))
        });

    // Combine routes and apply CORS
    let routes = connect.or(ws_route).with(cors);

    // Start the server
    println!("Starting server on 127.0.0.1:3030");
    println!("Allowed origin: {}", allowed_origin);
    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

async fn handle_connect(request: ConnectRequest, users: Users, correct_password: String) -> Result<impl Reply, Infallible> {
    if request.password != correct_password {
        return Ok(warp::reply::with_status("Incorrect password", warp::http::StatusCode::UNAUTHORIZED));
    }
    
    let users_count = users.read().await.len();
    if users_count >= MAX_USERS {
        return Ok(warp::reply::with_status("Room is full", warp::http::StatusCode::FORBIDDEN));
    }
    
    Ok(warp::reply::with_status("Connected", warp::http::StatusCode::OK))
}

// Handle new WebSocket connection
async fn user_connected(ws: WebSocket, users: Users, state: GlobalVideoState) {
    // Split the socket into a sender and receive of messages.
    let (user_ws_tx, mut user_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();

    // Use tokio spawn to handle the send stream
    tokio::task::spawn(UnboundedReceiverStream::new(rx)
        .forward(user_ws_tx)
        .map(|result| {
            if let Err(e) = result {
                eprintln!("websocket send error: {}", e);
            }
        })
    );

    // Generate a user ID and add the user to our map
    let user_id = generate_user_id();
    users.write().await.insert(user_id.clone(), tx.clone());

    println!("New user connected: {}", user_id);

    // Send current video state to the new user
    if let Some(current_state) = state.read().await.clone() {
        let _ = tx.send(Ok(Message::text(serde_json::to_string(&current_state).unwrap())));
    }

    // Handle incoming messages
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

    // User disconnected
    user_disconnected(user_id, &users).await;
}

// Handle a message from a user
async fn user_message(user_id: String, msg: Message, users: &Users, state: &GlobalVideoState) {
    // Try to get the message as a string
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    // Parse the message as VideoState
    let video_state: VideoState = match serde_json::from_str(msg) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("Error parsing message from {}: {}", user_id, e);
            return;
        }
    };

    println!("Received message from {}: {:?}", user_id, video_state);

    // Update global video state
    *state.write().await = Some(video_state.clone());

    // Prepare the message to send
    let new_msg = serde_json::to_string(&video_state).unwrap();

    // Send the message to all other users
    for (uid, tx) in users.read().await.iter() {
        if *uid != user_id {
            if let Err(_disconnected) = tx.send(Ok(Message::text(new_msg.clone()))) {
                // The tx is disconnected, our `user_disconnected` code
                // should be happening in another task, nothing more to
                // do here.
            }
        }
    }
}

// Handle a user disconnection
async fn user_disconnected(user_id: String, users: &Users) {
    users.write().await.remove(&user_id);
    println!("User disconnected: {}", user_id);
}

// Generate a random user ID
fn generate_user_id() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(7)
        .map(char::from)
        .collect()
}