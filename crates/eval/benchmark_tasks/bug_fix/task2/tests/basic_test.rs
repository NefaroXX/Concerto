use fix_linked_list::LinkedList;

#[test]
fn test_push_and_pop() {
    let mut list = LinkedList::new();
    list.push(1);
    list.push(2);
    list.push(3);
    assert_eq!(list.pop(), Some(3));
    assert_eq!(list.pop(), Some(2));
    assert_eq!(list.pop(), Some(1));
    assert_eq!(list.pop(), None);
}

#[test]
fn test_peek() {
    let mut list = LinkedList::new();
    assert_eq!(list.peek(), None);
    list.push(42);
    assert_eq!(list.peek(), Some(42));
    list.pop();
    assert_eq!(list.peek(), None);
}
