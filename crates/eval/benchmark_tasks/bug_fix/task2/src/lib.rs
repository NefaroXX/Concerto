pub struct Node {
    pub value: i32,
    pub next: *mut Node,
}

pub struct LinkedList {
    head: *mut Node,
}

impl LinkedList {
    pub fn new() -> Self {
        Self { head: std::ptr::null_mut() }
    }

    pub fn push(&mut self, value: i32) {
        let new_node = Box::into_raw(Box::new(Node {
            value,
            next: self.head,
        }));
        self.head = new_node;
    }

    pub fn pop(&mut self) -> Option<i32> {
        if self.head.is_null() {
            return None;
        }
        unsafe {
            let node = Box::from_raw(self.head);
            self.head = node.next;
            Some(node.value)
        }
    }

    /// Bug: dereferences null pointer when list is empty.
    pub fn peek(&self) -> Option<i32> {
        unsafe {
            Some((*self.head).value)
        }
    }
}
