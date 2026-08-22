use axum::{routing::{get, post, delete}, Router, Json, extract::Path};
use serde::{Serialize, Deserialize};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Todo {
    id: u64,
    text: String,
}

#[derive(Serialize)]
struct TodoList {
    todos: Vec<Todo>,
}

#[derive(Deserialize)]
struct CreateTodo {
    text: String,
}

type SharedState = Arc<Mutex<Vec<Todo>>>;

#[tokio::main]
async fn main() {
    let state = Arc::new(Mutex::new(Vec::<Todo>::new()));
    let app = Router::new()
        .route("/todos", get(list_todos).post(create_todo))
        .route("/todos/:id", delete(delete_todo))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    println!("Server running on http://127.0.0.1:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn list_todos(state: axum::extract::State<SharedState>) -> Json<TodoList> {
    let todos = state.lock().unwrap().clone();
    Json(TodoList { todos })
}

async fn create_todo(state: axum::extract::State<SharedState>, Json(payload): Json<CreateTodo>) -> Json<Todo> {
    let new_id = state.lock().unwrap().len() as u64 + 1;
    let todo = Todo { id: new_id, text: payload.text };

    state.lock().unwrap().push(todo.clone());
    Json(todo)
}

async fn delete_todo(state: axum::extract::State<SharedState>, Path(id): Path<u64>) -> Json<bool> {
    let mut todos = state.lock().unwrap();
    let len_before = todos.len();
    todos.retain(|t| t.id != id);
    let removed = todos.len() < len_before;
    Json(removed)
}
