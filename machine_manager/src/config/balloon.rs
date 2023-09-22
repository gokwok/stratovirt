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

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use super::{pci_args_check, ConfigCheck};
use crate::config::{check_arg_too_long, CmdParser, ConfigError, ExBool, VmConfig};

const MEM_BUFFER_PERCENT_MIN: u32 = 20;
const MEM_BUFFER_PERCENT_MAX: u32 = 80;
const MEM_BUFFER_PERCENT_DEFAULT: u32 = 50;
const MONITOR_INTERVAL_SECOND_MIN: u32 = 5;
const MONITOR_INTERVAL_SECOND_MAX: u32 = 300;
const MONITOR_INTERVAL_SECOND_DEFAULT: u32 = 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BalloonConfig {
    pub id: String,
    pub deflate_on_oom: bool,
    pub free_page_reporting: bool,
    pub auto_balloon: bool,
    pub membuf_percent: u32,
    pub monitor_interval: u32,
}

impl ConfigCheck for BalloonConfig {
    fn check(&self) -> Result<()> {
        check_arg_too_long(&self.id, "balloon id")?;

        if !self.auto_balloon {
            return Ok(());
        }
        if self.membuf_percent > MEM_BUFFER_PERCENT_MAX
            || self.membuf_percent < MEM_BUFFER_PERCENT_MIN
        {
            return Err(anyhow!(ConfigError::IllegalValue(
                "balloon membuf-percent".to_string(),
                MEM_BUFFER_PERCENT_MIN as u64,
                false,
                MEM_BUFFER_PERCENT_MAX as u64,
                false,
            )));
        }
        if self.monitor_interval > MONITOR_INTERVAL_SECOND_MAX
            || self.monitor_interval < MONITOR_INTERVAL_SECOND_MIN
        {
            return Err(anyhow!(ConfigError::IllegalValue(
                "balloon monitor-interval".to_string(),
                MONITOR_INTERVAL_SECOND_MIN as u64,
                false,
                MONITOR_INTERVAL_SECOND_MAX as u64,
                false,
            )));
        }
        Ok(())
    }
}

pub fn parse_balloon(vm_config: &mut VmConfig, balloon_config: &str) -> Result<BalloonConfig> {
    if vm_config.dev_name.get("balloon").is_some() {
        bail!("Only one balloon device is supported for each vm.");
    }
    let mut cmd_parser = CmdParser::new("virtio-balloon");
    cmd_parser
        .push("")
        .push("bus")
        .push("addr")
        .push("multifunction")
        .push("id")
        .push("deflate-on-oom")
        .push("free-page-reporting")
        .push("auto-balloon")
        .push("membuf-percent")
        .push("monitor-interval");
    cmd_parser.parse(balloon_config)?;

    pci_args_check(&cmd_parser)?;
    let mut balloon = BalloonConfig {
        membuf_percent: MEM_BUFFER_PERCENT_DEFAULT,
        monitor_interval: MONITOR_INTERVAL_SECOND_DEFAULT,
        ..Default::default()
    };

    if let Some(default) = cmd_parser.get_value::<ExBool>("deflate-on-oom")? {
        balloon.deflate_on_oom = default.into();
    }
    if let Some(default) = cmd_parser.get_value::<ExBool>("free-page-reporting")? {
        balloon.free_page_reporting = default.into();
    }
    if let Some(id) = cmd_parser.get_value::<String>("id")? {
        balloon.id = id;
    }
    if let Some(default) = cmd_parser.get_value::<ExBool>("auto-balloon")? {
        balloon.auto_balloon = default.into();
    }
    if let Some(membuf_percent) = cmd_parser.get_value::<u32>("membuf-percent")? {
        balloon.membuf_percent = membuf_percent;
    }
    if let Some(monitor_interval) = cmd_parser.get_value::<u32>("monitor-interval")? {
        balloon.monitor_interval = monitor_interval;
    }
    balloon.check()?;
    vm_config.dev_name.insert("balloon".to_string(), 1);
    Ok(balloon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::get_pci_bdf;

    #[test]
    fn test_balloon_config_cmdline_parser() {
        let mut vm_config = VmConfig::default();
        let bln_cfg_res = parse_balloon(
            &mut vm_config,
            "virtio-balloon-device,deflate-on-oom=true,id=balloon0",
        );
        assert!(bln_cfg_res.is_ok());
        let balloon_configs = bln_cfg_res.unwrap();
        assert_eq!(balloon_configs.id, "balloon0".to_string());
        assert_eq!(balloon_configs.deflate_on_oom, true);
    }

    #[test]
    fn test_pci_balloon_config_cmdline_parser() {
        let mut vm_config = VmConfig::default();
        let bln_cfg = "virtio-balloon-pci,deflate-on-oom=true,bus=pcie.0,addr=0x1.0x2,id=balloon0";
        let bln_cfg_res = parse_balloon(&mut vm_config, bln_cfg);
        assert!(bln_cfg_res.is_ok());
        let balloon_configs = bln_cfg_res.unwrap();
        assert_eq!(balloon_configs.id, "balloon0".to_string());
        assert_eq!(balloon_configs.deflate_on_oom, true);

        let pci_bdf = get_pci_bdf(bln_cfg);
        assert!(pci_bdf.is_ok());
        let pci = pci_bdf.unwrap();
        assert_eq!(pci.bus, "pcie.0".to_string());
        assert_eq!(pci.addr, (1, 2));

        let mut vm_config = VmConfig::default();
        let bln_cfg = "virtio-balloon-pci,deflate-on-oom=true,bus=pcie.0,addr=0x1.0x2,id=balloon0,multifunction=on";
        assert!(parse_balloon(&mut vm_config, bln_cfg).is_ok());
    }

    #[test]
    fn test_two_balloon_config_cmdline_parser() {
        let mut vm_config = VmConfig::default();
        let bln_cfg_res1 = parse_balloon(
            &mut vm_config,
            "virtio-balloon-device,deflate-on-oom=true,id=balloon0",
        );
        assert!(bln_cfg_res1.is_ok());
        let bln_cfg_res2 = parse_balloon(
            &mut vm_config,
            "virtio-balloon-device,deflate-on-oom=true,id=balloon1",
        );
        assert!(bln_cfg_res2.is_err());
    }

    #[test]
    fn test_fpr_balloon_config_cmdline_parser() {
        let mut vm_config1 = VmConfig::default();
        let bln_cfg_res1 = parse_balloon(
            &mut vm_config1,
            "virtio-balloon-device,free-page-reporting=true,id=balloon0",
        );
        assert!(bln_cfg_res1.is_ok());
        let balloon_configs1 = bln_cfg_res1.unwrap();
        assert_eq!(balloon_configs1.id, "balloon0".to_string());
        assert_eq!(balloon_configs1.free_page_reporting, true);

        let mut vm_config2 = VmConfig::default();
        let bln_cfg_res2 = parse_balloon(
            &mut vm_config2,
            "virtio-balloon-device,free-page-reporting=false,id=balloon0",
        );
        assert!(bln_cfg_res2.is_ok());
        let balloon_configs2 = bln_cfg_res2.unwrap();
        assert_eq!(balloon_configs2.id, "balloon0".to_string());
        assert_eq!(balloon_configs2.free_page_reporting, false);

        let mut vm_config3 = VmConfig::default();
        let bln_cfg_res3 = parse_balloon(
            &mut vm_config3,
            "virtio-balloon-pci,free-page-reporting=true,bus=pcie.0,addr=0x1.0x2,id=balloon0",
        );
        assert!(bln_cfg_res3.is_ok());
        let balloon_configs3 = bln_cfg_res3.unwrap();
        assert_eq!(balloon_configs3.id, "balloon0".to_string());
        assert_eq!(balloon_configs3.free_page_reporting, true);

        let mut vm_config4 = VmConfig::default();
        let bln_cfg_res4 = parse_balloon(
            &mut vm_config4,
            "virtio-balloon-pci,free-page-reporting=false,bus=pcie.0,addr=0x1.0x2,id=balloon0",
        );
        assert!(bln_cfg_res4.is_ok());
        let balloon_configs4 = bln_cfg_res4.unwrap();
        assert_eq!(balloon_configs4.id, "balloon0".to_string());
        assert_eq!(balloon_configs4.free_page_reporting, false);

        let mut vm_config5 = VmConfig::default();
        let bln_cfg_res5 = parse_balloon(&mut vm_config5, "virtio-balloon-device,id=balloon0");
        assert!(bln_cfg_res5.is_ok());
        let balloon_configs5 = bln_cfg_res5.unwrap();
        assert_eq!(balloon_configs5.id, "balloon0".to_string());
        assert_eq!(balloon_configs5.free_page_reporting, false);

        let mut vm_config6 = VmConfig::default();
        let bln_cfg_res6 = parse_balloon(
            &mut vm_config6,
            "virtio-balloon-device,free-page-reporting=2,id=balloon0",
        );
        assert!(bln_cfg_res6.is_err());
    }
}
