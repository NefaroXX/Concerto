use fix_binary_search::binary_search;

#[test]
fn test_binary_search_found() {
    let arr = [1, 3, 5, 7, 9, 11, 13];
    assert_eq!(binary_search(&arr, 7), Some(3));
    assert_eq!(binary_search(&arr, 1), Some(0));
    assert_eq!(binary_search(&arr, 13), Some(6));
}

#[test]
fn test_binary_search_not_found() {
    let arr = [1, 3, 5, 7, 9];
    assert_eq!(binary_search(&arr, 4), None);
    assert_eq!(binary_search(&arr, 10), None);
}

#[test]
fn test_binary_search_empty() {
    let arr: [i32; 0] = [];
    assert_eq!(binary_search(&arr, 1), None);
}

#[test]
fn test_binary_search_single_element() {
    let arr = [5];
    assert_eq!(binary_search(&arr, 5), Some(0));
    assert_eq!(binary_search(&arr, 3), None);
}
