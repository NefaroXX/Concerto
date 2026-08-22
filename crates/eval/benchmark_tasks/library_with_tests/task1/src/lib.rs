pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
            result.push(ch.to_lowercase().next().unwrap());
        } else if ch.is_uppercase() {
            result.push(ch.to_lowercase().next().unwrap());
        } else {
            result.push(ch);
        }
    }
    result
}

pub fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut next_upper = false;
    for ch in s.chars() {
        if ch == '_' || ch == '-' {
            next_upper = true;
        } else if next_upper {
            result.push(ch.to_uppercase().next().unwrap());
            next_upper = false;
        } else {
            result.push(ch);
        }
    }
    result
}

pub fn to_kebab_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
            result.push(ch.to_lowercase().next().unwrap());
        } else if ch.is_uppercase() {
            result.push(ch.to_lowercase().next().unwrap());
        } else if ch == '_' {
            result.push('-');
        } else {
            result.push(ch);
        }
    }
    result
}

pub fn from_snake_to_camel(s: &str) -> String {
    to_camel_case(s)
}

pub fn from_camel_to_snake(s: &str) -> String {
    to_snake_case(s)
}
