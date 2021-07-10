// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

const PCI_NUM_BARS: u8 = 6;
const PCI_ROM_SLOT: u8 = 6;

struct MsixTable {
    table_bar: u8,
    table_offset: u64,
    table_size: u64,
}

struct VfioMsixInfo {
    // Table bar, table offset and table size info.
    table: MsixTable,
    // Msix enteries.
    enteries: u16,
    // Vfio device irq info
    #[allow(dead_code)]
    vfio_irq: HashMap<u32, VfioIrq>,
}

struct VfioBar {
    vfio_region: VfioRegion,
    region_type: RegionType,
    size: u64,
}

struct GsiMsiRoute {
    irq_fd: Option<EventFd>,
    gsi: i32,
}

/// VfioPciDevice is a VFIO PCI device. It implements PciDevOps trait for a PCI device.
/// And it is bound to a VFIO device.
pub struct VfioPciDevice {
    pci_config: PciConfig,
    config_size: u64,
    // Offset of pci config space region within vfio device fd.
    config_offset: u64,
    // Vfio device which is bound to.
    vfio_device: Arc<VfioDevice>,
    // Cache of MSI-X setup.
    msix_info: Option<VfioMsixInfo>,
    // Bars information without ROM.
    vfio_bars: Arc<Mutex<Vec<VfioBar>>>,
    // Maintains a list of GSI with irqfds that are registered to kvm.
    gsi_msi_routes: Arc<Mutex<Vec<GsiMsiRoute>>>,
    devfn: u8,
    dev_id: u16,
    name: String,
    parent_bus: Weak<Mutex<PciBus>>,
}

impl VfioPciDevice {
    /// New a VFIO PCI device structure for the vfio device created by VMM.
    pub fn new(
        path: &Path,
        container: Arc<VfioContainer>,
        devfn: u8,
        name: String,
        parent_bus: Weak<Mutex<PciBus>>,
    ) -> PciResult<Self> {
        Ok(VfioPciDevice {
            // Unknown PCI or PCIe type here, allocate enough space to match the two types.
            pci_config: PciConfig::new(PCIE_CONFIG_SPACE_SIZE, PCI_NUM_BARS),
            config_size: 0,
            config_offset: 0,
            vfio_device: Arc::new(
                VfioDevice::new(container, path).chain_err(|| "Failed to new vfio device")?,
            ),
            msix_info: None,
            vfio_bars: Arc::new(Mutex::new(Vec::with_capacity(PCI_NUM_BARS as usize))),
            gsi_msi_routes: Arc::new(Mutex::new(Vec::new())),
            devfn,
            dev_id: 0,
            name,
            parent_bus,
        })
    }

    fn get_pci_config(&mut self) -> PciResult<()> {
        let argsz: u32 = size_of::<vfio::vfio_region_info>() as u32;
        let mut info = vfio::vfio_region_info {
            argsz,
            flags: 0,
            index: vfio::VFIO_PCI_CONFIG_REGION_INDEX,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };

        // Safe as device is the owner of file, and we will verify the result is valid.
        let ret = unsafe {
            ioctl_with_mut_ref(
                &self.vfio_device.device,
                VFIO_DEVICE_GET_REGION_INFO(),
                &mut info,
            )
        };
        if ret < 0 {
            return Err(ErrorKind::VfioIoctl("VFIO_GET_PCI_CONFIG_INFO".to_string(), ret).into());
        }

        self.config_size = info.size;
        self.config_offset = info.offset;
        let mut config_data = vec![0_u8; self.config_size as usize];
        self.vfio_device
            .read_region(config_data.as_mut_slice(), self.config_offset, 0)?;
        self.pci_config.config = config_data;

        Ok(())
    }

    /// Disable I/O, MMIO, bus master and INTx states, And clear host device bar size information.
    /// Guest OS can get residual addresses from the host if not clear bar size.
    fn pci_config_reset(&mut self) -> PciResult<()> {
        let mut cmd = le_read_u16(&self.pci_config.config, COMMAND as usize)?;
        cmd &= !(COMMAND_IO_SPACE
            | COMMAND_MEMORY_SPACE
            | COMMAND_BUS_MASTER
            | COMMAND_INTERRUPT_DISABLE);
        le_write_u16(&mut self.pci_config.config, COMMAND as usize, cmd)?;

        let mut data = vec![0u8; 2];
        LittleEndian::write_u16(&mut data, cmd);
        self.vfio_device
            .write_region(data.as_slice(), self.config_offset, COMMAND as u64)?;

        for i in 0..PCI_ROM_SLOT {
            let offset = BAR_0 as usize + REG_SIZE * i as usize;
            let v = le_read_u32(&self.pci_config.config, offset)?;
            if v & BAR_IO_SPACE as u32 != 0 {
                le_write_u32(&mut self.pci_config.config, offset, v & !IO_BASE_ADDR_MASK)?;
            } else {
                le_write_u32(
                    &mut self.pci_config.config,
                    offset,
                    v & !MEM_BASE_ADDR_MASK as u32,
                )?;
            }
        }

        Ok(())
    }

    /// Get MSI-X table, vfio_irq and entry information from vfio device.
    fn get_msix_info(&mut self) -> PciResult<VfioMsixInfo> {
        let n = self.vfio_device.clone().dev_info.num_irqs;
        let vfio_irq = self
            .vfio_device
            .get_irqs_info(n)
            .chain_err(|| "Failed to get vfio irqs info")?;

        let cap_offset = self.pci_config.find_pci_cap(MSIX_CAP_ID);
        let table = le_read_u32(
            &self.pci_config.config,
            cap_offset + MSIX_CAP_TABLE as usize,
        )?;

        let ctrl = le_read_u16(
            &self.pci_config.config,
            cap_offset + MSIX_CAP_CONTROL as usize,
        )?;
        let enteries = (ctrl & MSIX_TABLE_SIZE_MAX) + 1;
        // Make sure that if enteries less than 1 or greater than (0x7ff + 1) is error value.
        if !(1..=(MSIX_TABLE_SIZE_MAX + 1)).contains(&enteries) {
            bail!(
                "The number of MSI-X vectors is invalid, MSI-X vectors are {}",
                enteries,
            );
        }

        Ok(VfioMsixInfo {
            table: MsixTable {
                table_bar: (table as u16 & MSIX_TABLE_BIR) as u8,
                table_offset: (table & MSIX_TABLE_OFFSET) as u64,
                table_size: (enteries * MSIX_TABLE_ENTRY_SIZE) as u64,
            },
            enteries: enteries as u16,
            vfio_irq,
        })
    }

    /// Get vfio bars information. Vfio device won't allow to mmap the MSI-X table area,
    /// we need to separate MSI-X table area and region mmap area.
    fn bar_region_info(&mut self) -> PciResult<Vec<VfioBar>> {
        let mut vfio_bars: Vec<VfioBar> = Vec::new();
        let mut infos = self
            .vfio_device
            .get_regions_info()
            .chain_err(|| "Failed get vfio device regions info")?;

        for i in 0..PCI_ROM_SLOT {
            let mut data = vec![0_u8; 4];
            self.vfio_device.read_region(
                data.as_mut_slice(),
                self.config_offset,
                (BAR_0 + (REG_SIZE as u8) * i) as u64,
            )?;
            let mut region_type = RegionType::Mem32Bit;
            let pci_bar = LittleEndian::read_u32(&data);
            if pci_bar & BAR_IO_SPACE as u32 != 0 {
                region_type = RegionType::Io;
            } else if pci_bar & BAR_MEM_64BIT as u32 != 0 {
                region_type = RegionType::Mem64Bit;
            }
            let vfio_region = infos.remove(0);
            let size = vfio_region.size;

            vfio_bars.push(VfioBar {
                vfio_region,
                region_type,
                size,
            });
        }

        self.fixup_msix_region(&mut vfio_bars)?;

        Ok(vfio_bars)
    }

    fn fixup_msix_region(&self, vfio_bars: &mut Vec<VfioBar>) -> PciResult<()> {
        let msix_info = self
            .msix_info
            .as_ref()
            .chain_err(|| "Failed to get MSIX info")?;

        let vfio_bar = vfio_bars
            .get_mut(msix_info.table.table_bar as usize)
            .chain_err(|| "Failed to get vfio bar info")?;
        let region = &mut vfio_bar.vfio_region;
        // If MSI-X area already setups or does not support mapping, we shall just return.
        if region.mmaps.len() != 1
            || region.mmaps[0].offset != 0
            || region.size != region.mmaps[0].size
        {
            return Ok(());
        }

        // Align MSI-X table start and end to host page size.
        let page_size = host_page_size();
        let start: u64 = ((msix_info.table.table_offset as i64) & (0 - page_size as i64)) as u64;
        let end: u64 = (((msix_info.table.table_offset + msix_info.table.table_size)
            + (page_size - 1)) as i64
            & (0 - page_size as i64)) as u64;

        // The remaining area of the BAR before or after MSI-X table is remappable.
        if start == 0 {
            if end >= region.size {
                region.mmaps.clear();
            } else {
                region.mmaps[0].offset = end;
                region.mmaps[0].size = region.size - end;
            }
        } else if end >= region.size {
            region.mmaps[0].size = start;
        } else {
            region.mmaps[0].offset = 0;
            region.mmaps[0].size = start;
            region.mmaps.push(MmapInfo {
                offset: end,
                size: region.size - end,
            });
        }

        Ok(())
    }
}

impl PciDevOps for VfioPciDevice {
    fn init_write_mask(&mut self) -> PciResult<()> {
        self.pci_config.init_common_write_mask()
    }

    fn init_write_clear_mask(&mut self) -> PciResult<()> {
        self.pci_config.init_common_write_clear_mask()
    }

    fn realize(mut self) -> PciResult<()> {
        self.init_write_mask()?;
        self.init_write_clear_mask()?;
        self.vfio_device
            .reset()
            .chain_err(|| "Failed to reset vfio device")?;

        self.get_pci_config()
            .chain_err(|| "Failed to get vfio device pci config space")?;
        self.pci_config_reset()
            .chain_err(|| "Failed to reset vfio device pci config space")?;

        #[cfg(target_arch = "aarch64")]
        {
            let bus_num = self
                .parent_bus
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .number(SECONDARY_BUS_NUM as usize);
            self.dev_id = self.set_dev_id(bus_num, self.devfn);
        }

        self.msix_info = Some(
            self.get_msix_info()
                .chain_err(|| "Failed to get MSI-X info")?,
        );
        self.vfio_bars = Arc::new(Mutex::new(
            self.bar_region_info()
                .chain_err(|| "Fail to get bar region info")?,
        ));
        self.register_bars().chain_err(|| "Fail to register bars")?;

        let devfn = self.devfn;
        let dev = Arc::new(Mutex::new(self));
        let pci_bus = dev.lock().unwrap().parent_bus.upgrade().unwrap();
        let mut locked_pci_bus = pci_bus.lock().unwrap();
        let pci_device = locked_pci_bus.devices.get(&devfn);
        if pci_device.is_none() {
            locked_pci_bus.devices.insert(devfn, dev);
        } else {
            bail!(
                "Devfn {:?} has been used by {:?}",
                &devfn,
                pci_device.unwrap().lock().unwrap().name()
            );
        }

        Ok(())
    }

    /// Read pci data from pci config if it emulate, otherwise read from vfio device.
    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let size = data.len();
        let end = offset + size;
        if end > (self.config_size as usize) || size > 4 {
            error!(
                "Failed to read pci config space at offset {} with data size {}",
                offset, size
            );
            return;
        }

        if offset >= (BAR_0 as usize) && offset < (BAR_5 as usize) + REG_SIZE {
            self.pci_config.read(offset, data);
            return;
        }

        if let Err(e) = self
            .vfio_device
            .read_region(data, self.config_offset, offset as u64)
        {
            error!("Failed to read device pci config, error is {}", e);
            return;
        }
        for (i, data) in data.iter_mut().enumerate().take(size) {
            if i + offset == 0x3d {
                // Clear INIx
                *data &= 0;
            } else if i + offset == 0x0e {
                // Clear multi-function
                *data &= 0x7f;
            }
        }
    }

    /// Write data to pci config and vfio device at the same time
    fn write_config(&mut self, offset: usize, data: &[u8]) {
        let size = data.len();
        let end = offset + size;
        if end > (self.config_size as usize) || size > 4 {
            error!(
                "Failed to write pci config space at offset {} with data size {}",
                offset, size
            );
            return;
        }

        // Let vfio device filter data to write.
        if let Err(e) = self
            .vfio_device
            .write_region(data, self.config_offset, offset as u64)
        {
            error!("Failed to write device pci config, error is {}", e);
            return;
        }

        let mut cap_offset = 0;
        if let Some(msix) = &self.pci_config.msix {
            cap_offset = msix.lock().unwrap().msix_cap_offset as usize;
        }

        if ranges_overlap(offset, end, COMMAND as usize, COMMAND as usize + 4) {
            self.pci_config.write(offset, data, self.dev_id);

            if le_read_u32(&self.pci_config.config, offset).unwrap() & COMMAND_MEMORY_SPACE as u32
                != 0
            {
                let parent_bus = self.parent_bus.upgrade().unwrap();
                let locked_parent_bus = parent_bus.lock().unwrap();
                if let Err(e) = self.pci_config.update_bar_mapping(
                    #[cfg(target_arch = "x86_64")]
                    &locked_parent_bus.io_region,
                    &locked_parent_bus.mem_region,
                ) {
                    error!("Failed to update bar, error is {}", e.display_chain());
                    return;
                }
                drop(locked_parent_bus);

                if let Err(e) = self.setup_bars_mmap() {
                    error!("Failed to map bar regions, error is {}", e.display_chain());
                    return;
                }
            }
        } else if ranges_overlap(offset, end, BAR_0 as usize, (BAR_5 as usize) + REG_SIZE) {
            self.pci_config.write(offset, data, self.dev_id);

            if size == 4 && LittleEndian::read_u32(data) != 0xffff_ffff {
                let parent_bus = self.parent_bus.upgrade().unwrap();
                let locked_parent_bus = parent_bus.lock().unwrap();
                if let Err(e) = self.pci_config.update_bar_mapping(
                    #[cfg(target_arch = "x86_64")]
                    &locked_parent_bus.io_region,
                    &locked_parent_bus.mem_region,
                ) {
                    error!("Failed to update bar, error is {}", e.display_chain());
                    return;
                }
            }
        } else if ranges_overlap(offset, end, cap_offset, cap_offset + MSIX_CAP_SIZE as usize) {
            let was_enable = is_msix_enabled(cap_offset, &self.pci_config.config);
            self.pci_config.write(offset, data, self.dev_id);
            let is_enable = is_msix_enabled(cap_offset, &self.pci_config.config);

            if !was_enable && is_enable {
                if let Err(e) = self.vfio_enable_msix() {
                    error!("Failed to enable MSI-X, error is {}", e.display_chain());
                    return;
                }
            } else if was_enable && !is_enable {
                if let Err(e) = self.vfio_disable_msix() {
                    error!("Failed to disable MSI-X, error is {}", e.display_chain());
                    return;
                }
            }
        } else {
            self.pci_config.write(offset, data, self.dev_id);
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}
