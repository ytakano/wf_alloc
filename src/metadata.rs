use core::marker::PhantomData;
use core::ops::Deref;

/// Runtime-length slice stored as a raw pointer so allocator metadata can be
/// placement-initialized before any heap allocator exists.
pub struct RuntimeSlice<T> {
    ptr: *mut T,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T> RuntimeSlice<T> {
    /// # Safety
    /// `ptr` must point to `len` initialized `T` values that outlive this
    /// wrapper. The memory must not be moved while the allocator is in use.
    pub const unsafe fn from_raw_parts(ptr: *mut T, len: usize) -> Self {
        Self {
            ptr,
            len,
            _marker: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[T] {
        // SAFETY: guaranteed by constructor contract.
        unsafe { core::slice::from_raw_parts(self.ptr.cast_const(), self.len) }
    }
}

impl<T> Deref for RuntimeSlice<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

unsafe impl<T: Send> Send for RuntimeSlice<T> {}
unsafe impl<T: Sync> Sync for RuntimeSlice<T> {}
