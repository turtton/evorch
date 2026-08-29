//! 有界リングバッファを提供するモジュールです。

/// ADR 0012 に基づく bounded ring buffer。容量を超えた push は最古要素を drop する。
#[derive(Debug)]
pub struct RingBuffer<T> {
    buf: std::collections::VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    /// 新しいリングバッファを生成する。
    ///
    /// `capacity == 0` の場合は panic する。
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "RingBuffer capacity must be greater than zero"
        );
        Self {
            buf: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// 満杯なら最古要素を drop し、`Some(dropped)` を返す。余裕があれば `None` を返す。
    pub fn push(&mut self, item: T) -> Option<T> {
        let dropped = if self.is_full() {
            self.buf.pop_front()
        } else {
            None
        };
        self.buf.push_back(item);
        dropped
    }

    /// 最古から最新の順に要素を返すイテレータ。
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buf.iter()
    }

    /// 保持している要素数を返す。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 要素を保持していない場合は `true` を返す。
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 容量まで要素を保持している場合は `true` を返す。
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity
    }

    /// バッファの容量を返す。
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn push_below_capacity_preserves_insertion_order() {
        // Given: 空の容量3のリングバッファ
        let mut buffer = RingBuffer::new(3);

        // When: 容量未満の要素を追加する
        assert_eq!(buffer.push(1), None);
        assert_eq!(buffer.push(2), None);

        // Then: dropは発生せず、挿入順で保持される
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn overflow_drops_oldest_items_after_wraparound() {
        // Given: 容量3のリングバッファ
        let mut buffer = RingBuffer::new(3);

        // When: 容量を2つ超えて追加する
        for item in 1..=3 {
            assert_eq!(buffer.push(item), None);
        }
        assert_eq!(buffer.push(4), Some(1));
        assert_eq!(buffer.push(5), Some(2));

        // Then: 最古要素から順にdropされ、残りは最古から最新の順になる
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), [3, 4, 5]);
    }

    #[test]
    fn state_methods_follow_push_transitions() {
        // Given: 空の容量2のリングバッファ
        let mut buffer = RingBuffer::<i32>::new(2);

        // Then: 初期状態は空で、満杯ではない
        assert!(buffer.is_empty());
        assert!(!buffer.is_full());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), 2);

        // When: 1つ追加する
        assert_eq!(buffer.push(1), None);

        // Then: 空ではなく、まだ満杯ではない
        assert!(!buffer.is_empty());
        assert!(!buffer.is_full());
        assert_eq!(buffer.len(), 1);

        // When: 2つ目を追加する
        assert_eq!(buffer.push(2), None);

        // Then: 満杯になる
        assert!(!buffer.is_empty());
        assert!(buffer.is_full());
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    #[should_panic]
    fn new_with_zero_capacity_panics() {
        // Given / When: 容量0で生成する
        let _ = RingBuffer::<i32>::new(0);
    }
}
