use coreaudio_sys::*;

use std::ffi::CString;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

// Queue: A wrapper around `dispatch_queue_t`.
// ------------------------------------------------------------------------------------------------
#[derive(Debug)]
pub struct Queue(dispatch_queue_t);

impl Queue {
    pub fn new(label: &str) -> Self {
        const DISPATCH_QUEUE_SERIAL: dispatch_queue_attr_t =
            ptr::null_mut::<dispatch_queue_attr_s>();
        let label = CString::new(label).unwrap();
        let c_string = label.as_ptr();
        let queue = Self(unsafe { dispatch_queue_create(c_string, DISPATCH_QUEUE_SERIAL) });
        queue.set_context(Box::new(AtomicBool::new(false)));
        queue
    }

    pub fn run_async<F>(&self, work: F)
    where
        F: Send + FnOnce(),
    {
        let should_cancel = self.get_context::<AtomicBool>();
        async_dispatch(self.0, || {
            if should_cancel.map_or(false, |v| v.load(Ordering::SeqCst)) {
                return;
            }
            work();
        });
    }

    pub fn run_sync<F>(&self, work: F)
    where
        F: Send + FnOnce(),
    {
        let should_cancel = self.get_context::<AtomicBool>();
        sync_dispatch(self.0, || {
            if should_cancel.map_or(false, |v| v.load(Ordering::SeqCst)) {
                return;
            }
            work();
        });
    }

    pub fn run_final<F>(&self, work: F)
    where
        F: Send + FnOnce(),
    {
        let should_cancel = self.get_context::<AtomicBool>();
        sync_dispatch(self.0, || {
            work();
            should_cancel
                .expect("dispatch context should be allocated!")
                .store(true, Ordering::SeqCst);
        });
    }

    // The type `T` must be same as the `T` used in `set_context`
    fn get_context<T>(&self) -> Option<&mut T> {
        unsafe {
            let context = dispatch_get_context(
                mem::transmute::<dispatch_queue_t, dispatch_object_t>(self.0),
            ) as *mut T;
            context.as_mut()
        }
    }

    fn set_context<T>(&self, context: Box<T>) {
        unsafe {
            let queue = mem::transmute::<dispatch_queue_t, dispatch_object_t>(self.0);
            // Leak the context from Box.
            dispatch_set_context(queue, Box::into_raw(context) as *mut c_void);

            extern "C" fn finalizer<T>(context: *mut c_void) {
                // Retake the leaked context into box and then drop it.
                let _ = unsafe { Box::from_raw(context as *mut T) };
            }

            // The `finalizer` is only run if the `context` in `queue` is set by `dispatch_set_context`.
            dispatch_set_finalizer_f(queue, Some(finalizer::<T>));
        }
    }

    fn release(&self) {
        unsafe {
            // This will release the inner `dispatch_queue_t` asynchronously.
            // TODO: This is incredibly unsafe. Find another way to release the queue.
            dispatch_release(mem::transmute::<dispatch_queue_t, dispatch_object_t>(
                self.0,
            ));
        }
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        self.release();
    }
}

impl Clone for Queue {
    fn clone(&self) -> Self {
        // TODO: This is incredibly unsafe. Find another way to retain the queue.
        unsafe {
            dispatch_retain(mem::transmute::<dispatch_queue_t, dispatch_object_t>(
                self.0,
            ));
        }
        Self(self.0)
    }
}

// Low-level Grand Central Dispatch (GCD) APIs
// ------------------------------------------------------------------------------------------------
fn async_dispatch<F>(queue: dispatch_queue_t, work: F)
where
    F: Send + FnOnce(),
{
    let (closure, executor) = create_closure_and_executor(work);
    unsafe {
        dispatch_async_f(queue, closure, executor);
    }
}

fn sync_dispatch<F>(queue: dispatch_queue_t, work: F)
where
    F: Send + FnOnce(),
{
    let (closure, executor) = create_closure_and_executor(work);
    unsafe {
        dispatch_sync_f(queue, closure, executor);
    }
}

// Return an raw pointer to a (unboxed) closure and an executor that
// will run the closure (after re-boxing the closure) when it's called.
fn create_closure_and_executor<F>(closure: F) -> (*mut c_void, dispatch_function_t)
where
    F: FnOnce(),
{
    extern "C" fn closure_executer<F>(unboxed_closure: *mut c_void)
    where
        F: FnOnce(),
    {
        // Retake the leaked closure.
        let closure = unsafe { Box::from_raw(unboxed_closure as *mut F) };
        // Execute the closure.
        (*closure)();
        // closure is released after finishing this function call.
    }

    let closure = Box::new(closure); // Allocate closure on heap.
    let executor: dispatch_function_t = Some(closure_executer::<F>);

    (
        Box::into_raw(closure) as *mut c_void, // Leak the closure.
        executor,
    )
}

#[test]
fn run_tasks_in_order() {
    let mut visited = Vec::<u32>::new();

    // Rust compilter doesn't allow a pointer to be passed across threads.
    // A hacky way to do that is to cast the pointer into a value, then
    // the value, which is actually an address, can be copied into threads.
    let ptr = &mut visited as *mut Vec<u32> as usize;

    fn visit(v: u32, visited_ptr: usize) {
        let visited = unsafe { &mut *(visited_ptr as *mut Vec<u32>) };
        visited.push(v);
    };

    let queue = Queue::new("Run tasks in order");

    queue.run_sync(move || visit(1, ptr));
    queue.run_sync(move || visit(2, ptr));
    queue.run_async(move || visit(3, ptr));
    queue.run_async(move || visit(4, ptr));
    // Call sync here to block the current thread and make sure all the tasks are done.
    queue.run_sync(move || visit(5, ptr));

    assert_eq!(visited, vec![1, 2, 3, 4, 5]);
}

#[test]
fn run_final_task() {
    let mut visited = Vec::<u32>::new();

    {
        // Rust compilter doesn't allow a pointer to be passed across threads.
        // A hacky way to do that is to cast the pointer into a value, then
        // the value, which is actually an address, can be copied into threads.
        let ptr = &mut visited as *mut Vec<u32> as usize;

        fn visit(v: u32, visited_ptr: usize) {
            let visited = unsafe { &mut *(visited_ptr as *mut Vec<u32>) };
            visited.push(v);
        };

        let queue = Queue::new("Task after run_final will be cancelled");

        queue.run_sync(move || visit(1, ptr));
        queue.run_async(move || visit(2, ptr));
        queue.run_final(move || visit(3, ptr));
        queue.run_async(move || visit(4, ptr));
        queue.run_sync(move || visit(5, ptr));
    }
    // `queue` will be dropped asynchronously and then the `finalizer` of the `queue`
    // should be fired to clean up the `context` set in the `queue`.

    assert_eq!(visited, vec![1, 2, 3]);
}
