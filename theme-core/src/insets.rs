#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeInsets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl EdgeInsets {
    pub const fn all(v: i32) -> Self {
        Self::new(v, v, v, v)
    }

    pub const fn symmetric(horizontal: i32, vertical: i32) -> Self {
        Self::new(vertical, horizontal, vertical, horizontal)
    }

    pub const fn new(top: i32, right: i32, bottom: i32, left: i32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn horizontal(&self) -> i32 {
        self.left + self.right
    }

    pub const fn vertical(&self) -> i32 {
        self.top + self.bottom
    }
}
