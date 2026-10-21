//! Read-only recovery from the exact hash-gated Speech SDK's callback producers.
//! This follows indexed maps and typed functors; it never scans memory or invokes internals.
use crate::state::SavedCallback;
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::{
        Diagnostics::Debug::ReadProcessMemory,
        Memory::{
            MEM_COMMIT, MEM_IMAGE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
            VirtualQuery,
        },
        Threading::GetCurrentProcess,
    },
};

type Result<T> = std::result::Result<T, &'static str>;
struct Reader {
    process: HANDLE,
    core: usize,
}

impl Reader {
    fn read<const N: usize>(&self, address: usize) -> Result<[u8; N]> {
        if !(0x10000..0x8000_0000_0000).contains(&address) || N > 4096 {
            return Err("callback address is invalid");
        }
        let mut bytes = [0; N];
        let mut count = 0;
        if unsafe {
            ReadProcessMemory(
                self.process,
                address as _,
                bytes.as_mut_ptr().cast(),
                N,
                &mut count,
            )
        } == 0
            || count != N
        {
            return Err("callback memory is unavailable");
        }
        Ok(bytes)
    }
    fn pointer(&self, address: usize) -> Result<usize> {
        Ok(usize::from_le_bytes(self.read(address)?))
    }
    fn signed(&self, address: usize) -> Result<isize> {
        let value = i32::from_le_bytes(self.read(address)?);
        if value.unsigned_abs() > 65536 {
            return Err("interface displacement exceeds limit");
        }
        Ok(value as isize)
    }
    fn information(&self, address: usize) -> Result<MEMORY_BASIC_INFORMATION> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        if unsafe { VirtualQuery(address as _, &mut info, std::mem::size_of_val(&info)) } == 0
            || info.State != MEM_COMMIT
            || info.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0
        {
            return Err("callback memory protection is unsupported");
        }
        Ok(info)
    }
    fn image(&self, address: usize) -> Result<()> {
        let info = self.information(address)?;
        if info.AllocationBase as usize != self.core || info.Type != MEM_IMAGE {
            return Err("interface table belongs to a different module");
        }
        Ok(())
    }
    fn node(&self, mapping: usize, key: usize) -> Result<usize> {
        let head = self.pointer(mapping)?;
        if self.read::<1>(head + 0x19)? != [1] || !(1..=4096).contains(&self.pointer(mapping + 8)?)
        {
            return Err("SDK registry layout is unsupported");
        }
        let mut node = self.pointer(head + 8)?;
        for _ in 0..64 {
            if node == head || self.read::<1>(node + 0x19)? != [0] {
                return Err("SDK handle was not found");
            }
            let current = self.pointer(node + 0x20)?;
            if current == key {
                return Ok(node);
            }
            node = self.pointer(node + if key < current { 0 } else { 0x10 })?;
        }
        Err("SDK registry lookup exceeds limit")
    }
    fn snapshot(&self, reco: usize) -> Result<[Option<SavedCallback>; 7]> {
        let registry = self.pointer(self.core + 0x2031B0)?;
        let manager = self.pointer(self.node(registry, 0x2BB8136A)? + 0x28)?;
        let object = self.pointer(self.node(manager + 0x30, reco)? + 0x28)?;
        let vtable = self.pointer(object)?;
        self.image(vtable)?;
        if self.pointer(vtable + 8)? != self.core + 0xEE8F0 {
            return Err("SDK recognizer object is unsupported");
        }
        let vbtable = self.pointer(object + 8)?;
        self.image(vbtable)?;
        let adjusted = (object + 8)
            .checked_add_signed(self.signed(vbtable + 4)?)
            .ok_or("interface adjustment overflow")?;
        let table = self.pointer(adjusted)?;
        self.image(table)?;
        if self.pointer(table + 8)? != self.core + 0xF0F50 {
            return Err("SDK interface thunk is unsupported");
        }
        let interface = adjusted
            .checked_add_signed(-self.signed(adjusted - 4)?)
            .and_then(|v| v.checked_sub(0x728))
            .ok_or("interface adjustment overflow")?;
        let vbtable = self.pointer(interface + 8)?;
        self.image(vbtable)?;
        let events = (interface + 8)
            .checked_add_signed(self.signed(vbtable)?)
            .ok_or("event interface adjustment overflow")?;
        let mut values = [None; 7];
        // CallbackKind order: started, stopped, recognizing, recognized, canceled, speech start/end.
        for (index, offset) in [8, 0x78, 0x388, 0x3F8, 0x468, 0x2A8, 0x318]
            .into_iter()
            .enumerate()
        {
            let event = events + offset;
            match self.pointer(event + 0x10)? {
                0 => continue,
                1 => (),
                _ => return Err("SDK event has an unsupported listener count"),
            }
            let head = self.pointer(event + 8)?;
            let node = self.pointer(head + 8)?;
            if node == head || self.read::<1>(node + 0x19)? != [0] {
                return Err("SDK callback node is invalid");
            }
            let functor = self.pointer(node + 0x60)?;
            let expected = self.core + if index < 2 { 0x197CD8 } else { 0x197DD8 };
            if self.pointer(functor)? != expected || self.pointer(functor + 0x10)? != reco {
                return Err("SDK callback functor identity is unsupported");
            }
            let callback = SavedCallback {
                function: self.pointer(functor + 8)?,
                context: self.pointer(functor + 0x18)?,
            };
            let info = self.information(callback.function)?;
            if !matches!(
                info.Protect & 0xff,
                PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
            ) {
                return Err("SDK callback function is not executable");
            }
            values[index] = Some(callback);
        }
        Ok(values)
    }
}

pub(crate) fn callbacks(core: usize, reco: usize) -> Result<[Option<SavedCallback>; 7]> {
    let reader = Reader {
        process: unsafe { GetCurrentProcess() },
        core,
    };
    let first = reader.snapshot(reco)?;
    if first != reader.snapshot(reco)? {
        return Err("SDK callbacks changed during the read");
    }
    Ok(first)
}
