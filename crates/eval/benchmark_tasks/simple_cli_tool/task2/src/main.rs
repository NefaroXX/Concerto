use std::fs;
use std::io::{self, BufRead};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let file_path: Option<String> = args.get(1).cloned();
    if let Some(path) = file_path {
        let content = fs::read_to_string(path).unwrap_or_else(|_| String::new());
        let lines = content.lines().count();
        let words = content.split_whitespace().count();
        let chars = content.chars().count();
        println!("Lines: {}", lines);
        println!("Words: {}", words);
        println!("Characters: {}", chars);
    } else {
        let stdin = io::stdin();
        let lines_count = stdin.lock().lines().count();
        println!("Lines: {}", lines_count);
        println!("Words: 0 (stdin mode not fully implemented)");
        println!("Characters: 0 (stdin mode not fully implemented)");
    }
}
