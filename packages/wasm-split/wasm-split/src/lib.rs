use std::{
    cell::Cell,
    ffi::c_void,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    thread::LocalKey,
};

pub use wasm_split_macro::{lazy_loader, wasm_split};

pub type Result<T> = std::result::Result<T, SplitLoaderError>;

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SplitLoaderError {
    FailedToLoad,
}
impl std::fmt::Display for SplitLoaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitLoaderError::FailedToLoad => write!(f, "Failed to load wasm-split module"),
        }
    }
}

/// A lazy loader that can be used to load a function from a split out `.wasm` file.
///
/// # Example
///
/// To use the split loader, you must first create the loader using the `lazy_loader` macro. This macro
/// requires the complete signature of the function you want to load. The extern abi string denotes
/// which module the function should be loaded from. If you don't know which module to use, use `auto`
/// and wasm-split will automatically combine all the modules into one.
///
/// ```rust, ignore
/// static LOADER: wasm_split::LazyLoader<Args, Ret> = wasm_split::lazy_loader!(extern "auto" fn SomeFunction(args: Args) -> Ret);
///
/// fn SomeFunction(args: Args) -> Ret {
///     // Implementation
/// }
/// ```
///
/// ## The `#[component(lazy)]` macro
///
/// If you're using wasm-split with Dioxus, the `#[component(lazy)]` macro is provided that wraps
/// the lazy loader with suspense. This means that the component will suspense until its body has
/// been loaded.
///
/// ```rust, ignore
/// fn app() -> Element {
///     rsx! {
///         Suspense {
///             fallback: rsx! { "Loading..." },
///             LazyComponent { abc: 0 }
///         }
///     }
/// }
///
/// #[component(lazy)]
/// fn LazyComponent(abc: i32) -> Element {
///     rsx! {
///         div {
///             "This is a lazy component! {abc}"
///         }
///     }
/// }
/// ```
pub struct LazyLoader<Args, Ret> {
    imported: unsafe extern "C" fn(arg: Args) -> Ret,
    key: &'static LocalKey<LazySplitLoader>,
}

impl<Args, Ret> LazyLoader<Args, Ret> {
    /// Create a new lazy loader from a lazy imported function and a LazySplitLoader
    ///
    /// # Safety
    /// This is unsafe because we're taking an arbitrary function pointer and using it as the loader.
    /// This function is likely not instantiated when passed here, so it should never be called directly.
    #[doc(hidden)]
    pub const unsafe fn new(
        imported: unsafe extern "C" fn(arg: Args) -> Ret,
        key: &'static LocalKey<LazySplitLoader>,
    ) -> Self {
        Self { imported, key }
    }

    /// Create a new lazy loader that is already resolved.
    pub const fn preloaded(f: fn(Args) -> Ret) -> Self {
        let imported =
            unsafe { std::mem::transmute::<fn(Args) -> Ret, unsafe extern "C" fn(Args) -> Ret>(f) };

        thread_local! {
            static LAZY: LazySplitLoader = LazySplitLoader::preloaded();
        };

        Self {
            imported,
            key: &LAZY,
        }
    }

    /// Load the lazy loader, returning an boolean indicating whether it loaded successfully
    pub async fn load(&'static self) -> bool {
        *self.key.with(|inner| inner.lazy.clone()).as_ref().await
    }

    /// Call the lazy loader with the given arguments
    pub fn call(&'static self, args: Args) -> Result<Ret> {
        let Some(true) = self.key.with(|inner| inner.lazy.try_get().copied()) else {
            return Err(SplitLoaderError::FailedToLoad);
        };

        Ok(unsafe { (self.imported)(args) })
    }
}

type Lazy = async_once_cell::Lazy<bool, SplitLoaderFuture>;
type LoadCallbackFn = unsafe extern "C" fn(*const c_void, bool) -> ();
type LoadFn = unsafe extern "C" fn(LoadCallbackFn, *const c_void) -> ();

pub struct LazySplitLoader {
    lazy: Pin<Rc<Lazy>>,
}

impl LazySplitLoader {
    /// Create a new lazy split loader from a load function that is generated by the wasm-split macro
    ///
    /// # Safety
    ///
    /// This is unsafe because we're taking an arbitrary function pointer and using it as the loader.
    /// It is likely not instantiated when passed here, so it should never be called directly.
    #[doc(hidden)]
    pub unsafe fn new(load: LoadFn) -> Self {
        Self {
            lazy: Rc::pin(Lazy::new({
                SplitLoaderFuture {
                    loader: Rc::new(SplitLoader {
                        state: Cell::new(SplitLoaderState::Deferred(load)),
                        waker: Cell::new(None),
                    }),
                }
            })),
        }
    }

    fn preloaded() -> Self {
        Self {
            lazy: Rc::pin(Lazy::new({
                SplitLoaderFuture {
                    loader: Rc::new(SplitLoader {
                        state: Cell::new(SplitLoaderState::Completed(true)),
                        waker: Cell::new(None),
                    }),
                }
            })),
        }
    }

    /// Wait for the lazy loader to load
    pub async fn ensure_loaded(loader: &'static std::thread::LocalKey<LazySplitLoader>) -> bool {
        *loader.with(|inner| inner.lazy.clone()).as_ref().await
    }
}

struct SplitLoader {
    state: Cell<SplitLoaderState>,
    waker: Cell<Option<Waker>>,
}

#[derive(Clone, Copy)]
enum SplitLoaderState {
    Deferred(LoadFn),
    Pending,
    Completed(bool),
}

struct SplitLoaderFuture {
    loader: Rc<SplitLoader>,
}

impl Future for SplitLoaderFuture {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        unsafe extern "C" fn load_callback(loader: *const c_void, success: bool) {
            let loader = unsafe { Rc::from_raw(loader as *const SplitLoader) };
            loader.state.set(SplitLoaderState::Completed(success));
            if let Some(waker) = loader.waker.take() {
                waker.wake()
            }
        }

        match self.loader.state.get() {
            SplitLoaderState::Deferred(load) => {
                self.loader.state.set(SplitLoaderState::Pending);
                self.loader.waker.set(Some(cx.waker().clone()));
                unsafe {
                    load(
                        load_callback,
                        Rc::<SplitLoader>::into_raw(self.loader.clone()) as *const c_void,
                    )
                };
                Poll::Pending
            }
            SplitLoaderState::Pending => {
                self.loader.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
            SplitLoaderState::Completed(value) => Poll::Ready(value),
        }
    }
}
