//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT, USER_STACK_SIZE};
use crate::sync::UPSafeCell;
use crate::task::current_task;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// a memory set instance through lazy_static! managing kernel space
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}

/// memory set structure, controls virtual-memory space
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
}

impl MemorySet {
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some((idx, area)) = self
            .areas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.vpn_range.get_start() == start_vpn)
        {
            area.unmap(&mut self.page_table);
            self.areas.remove(idx);
        }
    }
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }
    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        info!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        info!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping physical memory");
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set
    }
    /// Include sections in elf and trampoline and TrapContext and user stack,
    /// also returns user_sp and entry point.
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                memory_set.push(
                    map_area,
                    Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // map user stack with U flags
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // map TrapContext
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize,
        )
    }
    /// Copy an identical user_space
    pub fn from_existed_user(user_space: &MemorySet) -> MemorySet {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // copy data sections/trap_context/user_stack
        for area in user_space.areas.iter() {
            let new_area = MapArea::from_another(area);
            memory_set.push(new_area, None);
            // copy data from another space
            for vpn in area.vpn_range {
                let src_ppn = user_space.translate(vpn).unwrap().ppn();
                let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                dst_ppn
                    .get_bytes_array()
                    .copy_from_slice(src_ppn.get_bytes_array());
            }
        }
        memory_set
    }
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            core::arch::asm!("sfence.vma");
        }
    }
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    pub fn recycle_data_pages(&mut self) {
        //*self = Self::new_bare();
        self.areas.clear();
    }
    pub fn mmap(&mut self, start: usize, end: usize, prot: usize) -> isize {
        let (lvpn, rvpn) = (VirtAddr::from(start).floor(), VirtAddr::from(end).ceil());
        let range = VPNRange::new(lvpn, rvpn);

        self.areas.iter().for_each(|area| {
            info!("l, r, {:?}, {:?}", area.vpn_range.get_start(), area.vpn_range.get_end());
        });
        info!(
            "[map]: lvpn: {:?}, rvpn: {:?}, start: {:#x},end: {:#x}, pt: {:#x}",
            lvpn,
            rvpn,
            start,
            end,
            self.page_table.token()
        );
        if self
            .areas
            .iter()
            .any(|area| area.vpn_range.get_end() > area.vpn_range.get_start() && lvpn < area.vpn_range.get_end() && rvpn > area.vpn_range.get_start())
        {
            // [start, end)
            println!("already mapped");
            info!("end,{:?}",self.page_table.translate(rvpn).unwrap().ppn());
            return -1;
        }
        let mut permission = MapPermission::from_bits((prot as u8) << 1).unwrap();
        permission.set(MapPermission::U, true);

        self.insert_framed_area(lvpn.into(), rvpn.into(), permission);

        info!("[map] [test] ");
        range.into_iter().for_each(|vpn| {
            match self.translate(vpn) {
                Some(v) => info!("yes {:?}", v.ppn()),
                None => info!("male"),
            };
        });
        // self.areas.iter().for_each(|area| {
        //     let (lvpn, rvpn) = (area.get_start(), area.get_end());
        //     info!(
        //         "l, r, {:?}, {:?}, {:?}, {:?}",
        //         area.get_start(),
        //         area.get_end(),
        //         self.translate(lvpn).unwrap().ppn(),
        //         self.translate(rvpn).unwrap().ppn()
        //     );
        // });
        // show_frame_status();
        0
    }
    pub fn munmap(&mut self, start: usize, end: usize) -> isize {
        println!("unmap!!!,start: {:#x}, end: {:#x}", start, end);
        let (lvpn, rvpn) = (VirtAddr::from(start).floor(), VirtAddr::from(end).ceil());
        let range = VPNRange::new(lvpn, rvpn);
        // println!("unmap!!!");
        if self
            .areas
            .iter()
            .filter_map(|area| {
                let (start, end) = (area.vpn_range.get_start(), area.vpn_range.get_end());
                if start >= lvpn && end <= rvpn {
                    Some(end.0 - start.0)
                } else {
                    None
                }
            })
            .sum::<usize>()
            < (rvpn.0 - lvpn.0)
        {
            println!("already mapped");
            return -1;
        }
        // if range
        //     .into_iter()
        //     .any(|vpn|
        //         match self.page_table.translate(vpn) {
        //             Some(v) => {
        //                 // println!("?1: {:?}, {:?}", vpn, v.ppn());
        //                 // if v.ppn().0 == 0x0 {
        //                 //     true
        //                 // } else {
        //                     false
        //                 // }
        //             }
        //             None => true,}
        //     )
        // {
        //     info!("[remove frame] not");
        //     return -1;
        // }
        // info!("unmap!!! real pt: {:#x}", self.page_table.token());
        // self.areas = self
        //     .areas
        //     .to_owned()
        //     .into_iter()
        //     .filter_map(|mut area| {
        //         show_frame_status();
        //         因为自动drop会导致回收行为，丢失所有权就寄了
        //         let l = area.get_start();
        //         let r = area.get_end();
        //         info!(
        //             "[unmap] [find]: l: {:?}, r: {:?}, start: {:?}, end: {:?}",
        //             l, r, start, end
        //         );
        //         if l < r && start <= l && r <= end {
        //             info!("[unmap]: success,l,r:({:?}, {:?})", l, r);
        //             match self.translate(l) {
        //                 Some(v) => info!("male {:?}", v.ppn()),
        //                 None => info!("yes"),
        //             }
        //             area.unmap(&mut self.page_table);
        //             None
        //         } else {
        //             Some(area)
        //         }
        //     })
        //     .collect::<Vec<MapArea>>();
        let pte = &mut self.page_table;
        self.areas.iter_mut().for_each(|area| {
            let l = area.vpn_range.get_start();
            let r = area.vpn_range.get_end();
            info!(
                "[unmap] [find]: l: {:?}, r: {:?}, start: {:?}, end: {:?}",
                l, r, lvpn, rvpn
            );
            if lvpn <= l && r <= rvpn {
                info!("[unmap]: success,l,r:({:?}, {:?})", l, r);
                // match self.translate(l) {
                //     Some(v) => info!("male {:?}", v.ppn()),
                //     None => info!("yes"),
                // }
                area.unmap(pte);
                area.vpn_range = VPNRange::new(l, l);
            }
        });
        self.areas.retain(|area| area.vpn_range.get_start() < area.vpn_range.get_end());
        info!("[unmap] [test] ");
        self.areas.iter().for_each(|area| {
            info!("l, r, {:?}, {:?}", area.vpn_range.get_start(), area.vpn_range.get_end());
        });
        range.into_iter().for_each(|vpn| match self.translate(vpn) {
            Some(v) => info!("male {:?}, {:?}", vpn, v.ppn()),
            None => info!("yes"),
        });
        0
    }
      
}

/// map area structure, controls a contiguous piece of virtual memory
pub struct MapArea {
    vpn_range: VPNRange,
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    pub fn from_another(another: &MapArea) -> Self {
        Self {
            vpn_range: VPNRange::new(another.vpn_range.get_start(), another.vpn_range.get_end()),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
        }
    }
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }

    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        #[allow(clippy::single_match)]
        match self.map_type {
            MapType::Framed => {
                self.data_frames.remove(&vpn);
            }
            _ => {}
        }
        page_table.unmap(vpn);
    }
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// map type for memory set: identical or framed
pub enum MapType {
    Identical,
    Framed,
}

pub fn mmap(start: usize, len: usize, prot: usize) -> isize {
    if len == 0 {
        info!("reason1");
        return 0;
    }
    // 0，1，2位有效，其他位必须为0,mask => b 0...0111 =>0x7
    if (prot >> 3) != 0 || (prot & 0x7) == 0 || start % 4096 != 0 {
        info!("reason2");
        return -1;
    }
    if let Some(cur_tcb) = current_task() {
        let mut inner = cur_tcb.inner_exclusive_access();
        let end = start + len;
        println!("mmap!!!");
        inner.memory_set.mmap(start, end, prot)
    } else {
        -1
    }
}

pub fn munmap(start: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if start % 4096 != 0 {
        return -1;
    }
    if let Some(cur_tcb) = current_task() {
        let mut inner = cur_tcb.inner_exclusive_access();
        inner.memory_set.munmap(start, start + len)
    } else {
        -1
    }
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}

#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable());
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable());
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable());
    info!("remap_test passed!");
}
