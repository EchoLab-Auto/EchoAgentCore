//! 进程级唯一引导单元（P1 过渡机制；sole whitelisted bootstrap cell）。
//!
//! 解耦计划 Phase 1（`document/decoupling-plan.md` §8）与棘轮门禁
//! （`source/core/tests/no_global_state.rs`）的**唯一白名单**：全部进程级
//! 单例（原各文件的 `OnceLock` 静态）的存储收敛到本文件，经 `TypeId`
//! 键控的类型擦除表统一存取。各 getter/setter 保留原函数名与签名，
//! 只改函数体。
//!
//! 语义约定：
//! - [`set`] 首次生效（后续调用静默忽略），等价旧 `OnceLock::set`；
//! - [`get_or_init`] 惰性构造（锁内 double-check，init 只执行一次）；
//! - [`get`] 未设置 = `None`。读出值均为 clone（调用方拿不到内部借用）。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

struct Cell {
    map: RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

static KERNEL: OnceLock<Cell> = OnceLock::new();

fn cell() -> &'static Cell {
    KERNEL.get_or_init(|| Cell {
        map: RwLock::new(HashMap::new()),
    })
}

fn read_map() -> std::sync::RwLockReadGuard<'static, HashMap<TypeId, Box<dyn Any + Send + Sync>>> {
    cell()
        .map
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_map() -> std::sync::RwLockWriteGuard<'static, HashMap<TypeId, Box<dyn Any + Send + Sync>>>
{
    cell()
        .map
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 类型擦除表内的引用下行（TypeId 由调用方保证一致）。
fn typed<T: Any>(map: &HashMap<TypeId, Box<dyn Any + Send + Sync>>, id: TypeId) -> Option<&T> {
    let boxed = map.get(&id)?;
    let any: &(dyn Any + Send + Sync) = &**boxed;
    let any: &dyn Any = any;
    any.downcast_ref::<T>()
}

/// 写入进程级单例：**首次生效**（后续调用静默忽略，等价旧 OnceLock::set 语义）。返回是否本次生效。
pub fn set<T: Any + Send + Sync>(value: T) -> bool {
    let mut map = write_map();
    let id = TypeId::of::<T>();
    if map.contains_key(&id) {
        return false;
    }
    map.insert(id, Box::new(value));
    true
}

/// 惰性单例：首次调用执行 init，之后返回既有值（clone 出）。
pub fn get_or_init<T: Any + Send + Sync + Clone>(init: impl FnOnce() -> T) -> T {
    let id = TypeId::of::<T>();
    {
        let map = read_map();
        if let Some(value) = typed::<T>(&map, id) {
            return value.clone();
        }
    }
    let mut map = write_map();
    // 锁内 double-check：并发首调时只有一个线程执行 init。
    if let Some(value) = typed::<T>(&map, id) {
        return value.clone();
    }
    let value = init();
    map.insert(id, Box::new(value.clone()));
    value
}

/// 读取（未设置 = None）。
pub fn get<T: Any + Send + Sync + Clone>() -> Option<T> {
    let map = read_map();
    typed::<T>(&map, TypeId::of::<T>()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 每个用例用独立 newtype 作键：KERNEL 是进程级的，用例并行运行时
    // 不允许类型相撞。

    #[derive(Clone, Debug, PartialEq)]
    struct FirstWins(u32);

    #[test]
    fn set_takes_effect_only_once() {
        assert!(set(FirstWins(1)));
        assert!(!set(FirstWins(2)));
        assert_eq!(get::<FirstWins>(), Some(FirstWins(1)));
    }

    #[derive(Clone, Debug, PartialEq)]
    struct IsolatedA(u32);

    #[derive(Clone, Debug, PartialEq)]
    struct IsolatedB(String);

    #[test]
    fn different_types_are_isolated() {
        assert!(set(IsolatedA(7)));
        assert!(set(IsolatedB("b".into())));
        assert_eq!(get::<IsolatedA>(), Some(IsolatedA(7)));
        assert_eq!(get::<IsolatedB>(), Some(IsolatedB("b".into())));
        assert_eq!(get::<u64>(), None);
    }

    #[derive(Clone, Debug, PartialEq)]
    struct LazyOnce(&'static str);

    #[test]
    fn get_or_init_constructs_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let first = get_or_init(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            LazyOnce("first")
        });
        let second = get_or_init(|| {
            calls.fetch_add(1, Ordering::SeqCst); // 不应执行
            LazyOnce("second")
        });
        assert_eq!(first, LazyOnce("first"));
        assert_eq!(second, LazyOnce("first"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone, Debug, PartialEq)]
    struct SetThenInit(u8);

    #[derive(Clone, Debug, PartialEq)]
    struct InitThenSet(u8);

    #[test]
    fn set_and_get_or_init_are_compatible() {
        // set 先行：get_or_init 取既有值，init 不执行。
        assert!(set(SetThenInit(9)));
        assert_eq!(get_or_init(|| SetThenInit(0)), SetThenInit(9));
        // get_or_init 落值后 set 不再生效。
        assert_eq!(get_or_init(|| InitThenSet(3)), InitThenSet(3));
        assert!(!set(InitThenSet(4)));
        assert_eq!(get::<InitThenSet>(), Some(InitThenSet(3)));
    }

    #[derive(Clone)]
    struct Concurrent(Arc<AtomicUsize>);

    #[test]
    fn concurrent_access_does_not_panic() {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    let _ = set(Concurrent(Arc::new(AtomicUsize::new(i))));
                    let value = get_or_init(|| Concurrent(Arc::new(AtomicUsize::new(0))));
                    value.0.fetch_add(1, Ordering::SeqCst);
                    assert!(get::<Concurrent>().is_some());
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .expect("kernel cell concurrent access panicked");
        }
        assert!(get::<Concurrent>().is_some());
    }
}
