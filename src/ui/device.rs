//! 设备信息页：iPhone 身份 6 字段 + 整套随机 + 持久化（只模拟 iOS）。
//! ⚠ 设备 ID 固定复用（防 10121 风控）；随机后必须手动「保存」才生效。

use super::{mobile, theme, App};
use crate::crypto::header::HeaderIdentity;
use eframe::egui;

#[derive(Default)]
pub struct DevicePage {
    saved_flash: f32,
}

impl App {
    pub fn draw_device(&mut self, ui: &mut egui::Ui) {
        let mut page = std::mem::take(&mut self.device_page);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_device_inner(ui, &mut page));
        self.device_page = page;
    }

    fn draw_device_inner(&mut self, ui: &mut egui::Ui, page: &mut DevicePage) {
        ui.add_space(6.0);

        // 身份文件损坏：这里必须能自助修复，否则整个工具都不可用。
        if let Some(err) = self.identity_error.clone() {
            ui.colored_label(theme::err(), format!("× 设备身份读取失败：{err}"));
            ui.colored_label(
                theme::warn(),
                "原文件会改名保留（identity.json.corrupt-<时间戳>），随后重新生成 DeviceId/IDFA。",
            );
            mobile::row(ui, |ui| {
                if ui
                    .add_enabled(
                        !self.busy(super::JobKind::Login),
                        theme::primary_btn("备份并重建身份"),
                    )
                    .clicked()
                {
                    self.rebuild_identity();
                }
            });
            ui.separator();
        }

        mobile::row(ui, |ui| {
            if ui.button("随机生成").clicked() {
                randomize(&mut self.device_buf);
                self.status = "已生成随机 iPhone（未保存——手动保存后才生效）".into();
            }
        });

        ui.add_space(6.0);
        let id_label = "DeviceId（UUID 大写）";
        let idfa_label = "IDFA（可空；留空则按 DeviceId 派生并随保存落盘）";
        let name_label = "设备名";
        if mobile::compact_ui(ui) {
            ui.vertical(|ui| {
                ui.label(id_label);
                mobile::text_edit(ui, "device_id", &mut self.device_buf.device_id, crate::platform::InputKind::Text, ui.available_width());
                ui.label(idfa_label);
                mobile::text_edit(ui, "device_idfa", &mut self.device_buf.idfa, crate::platform::InputKind::Text, ui.available_width());
                ui.label("系统版本：");
                mobile::text_edit(ui, "device_os", &mut self.device_buf.os_version, crate::platform::InputKind::Text, ui.available_width());
                ui.label(name_label);
                mobile::text_edit(ui, "device_name", &mut self.device_buf.device_name, crate::platform::InputKind::Text, ui.available_width());
                ui.label("城市：");
                mobile::text_edit(ui, "device_city", &mut self.device_buf.city, crate::platform::InputKind::Text, ui.available_width());
                ui.label("定位锚点纬度：");
                mobile::drag_f64(ui, "device_lat", &mut self.device_buf.anchor_lat, -90.0..=90.0, 0.00001, 6);
                ui.label("定位锚点经度：");
                mobile::drag_f64(ui, "device_lon", &mut self.device_buf.anchor_lon, -180.0..=180.0, 0.00001, 6);
            });
        } else {
            egui::Grid::new("device_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(id_label);
                    mobile::text_edit(ui, "device_id", &mut self.device_buf.device_id, crate::platform::InputKind::Text, 340.0);
                    ui.end_row();
                    ui.label(idfa_label);
                    mobile::text_edit(ui, "device_idfa", &mut self.device_buf.idfa, crate::platform::InputKind::Text, 340.0);
                    ui.end_row();
                    ui.label("系统版本：");
                    mobile::text_edit(ui, "device_os", &mut self.device_buf.os_version, crate::platform::InputKind::Text, 120.0);
                    ui.end_row();
                    ui.label(name_label);
                    mobile::text_edit(ui, "device_name", &mut self.device_buf.device_name, crate::platform::InputKind::Text, 200.0);
                    ui.end_row();
                    ui.label("城市：");
                    mobile::text_edit(ui, "device_city", &mut self.device_buf.city, crate::platform::InputKind::Text, 120.0);
                    ui.end_row();
                    ui.label("定位锚点纬度：");
                    mobile::drag_f64(ui, "device_lat", &mut self.device_buf.anchor_lat, -90.0..=90.0, 0.00001, 6);
                    ui.end_row();
                    ui.label("定位锚点经度：");
                    mobile::drag_f64(ui, "device_lon", &mut self.device_buf.anchor_lon, -180.0..=180.0, 0.00001, 6);
                    ui.end_row();
                });
        }

        ui.add_space(4.0);
        // 分辨率随机型走：用户改不了，但要看得到，否则「保存后分辨率自己变了」无从解释。
        ui.colored_label(
            theme::text_dim(),
            format!(
                "分辨率（随设备名自动校正，只读）：逻辑 {} / 物理 {}",
                self.device_buf.logic_pixel, self.device_buf.physic_pixel
            ),
        );
        ui.colored_label(
            theme::text_dim(),
            "定位锚点填写顺序：先填纬度，再填经度；地图常见的“经度,纬度”格式需要调换后填写。",
        );
        ui.colored_label(
            theme::text_dim(),
            "修改城市或定位锚点后，请点击“保存”；跑步流程只使用已保存的身份信息。",
        );
        if location_fields_changed(&self.identity, &self.device_buf) {
            ui.colored_label(
                theme::warn(),
                "检测到城市或定位锚点有未保存修改，当前跑步仍会使用上次保存的值。",
            );
        }

        ui.add_space(8.0);
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("保存")).clicked() {
                // 机型命中真机池时按池内权威分辨率纠正：手改设备名后像素不会停在旧值上。
                let pixels_fixed = self.device_buf.normalize_pixels_to_model();
                // 留空的 IDFA 在保存时立即派生并落盘，而不是等下次启动再随机一个。
                if self.device_buf.idfa.trim().is_empty() {
                    let derived = self.device_buf.effective_idfa();
                    self.device_buf.idfa = derived;
                }
                match crate::api::model::save_identity(&self.device_buf) {
                    Ok(()) => {
                        self.identity = self.device_buf.clone();
                        self.status = "√ 设备身份已保存并生效".into();
                        self.log.push("√  已更新（新设备 ID 从下个请求开始生效）");
                        if pixels_fixed {
                            self.log.push(&format!(
                                "√  已按机型「{}」纠正分辨率为 {} / {}",
                                self.identity.device_name,
                                self.identity.logic_pixel,
                                self.identity.physic_pixel
                            ));
                        }
                        // 锚点/城市可能已换：让跑步页的路线预览失效，下次进入按新锚点重算。
                        self.run_page.preview_stale = true;
                        page.saved_flash = 2.0;
                    }
                    Err(e) => self.status = format!("× 保存失败：{e}"),
                }
            }
            if ui.button("撤销修改").clicked() {
                self.device_buf = self.identity.clone();
            }
            if page.saved_flash > 0.0 {
                ui.colored_label(theme::ok(), "已保存");
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.colored_label(
            theme::warn(),
            "风控提示：设备 ID 参与服务端会话校验，固定复用可显著降低 10121（设备风险）概率；",
        );
        ui.colored_label(
            theme::warn(),
            "不要每次启动都换设备；「随机生成」只改界面缓冲区，手动「保存」后才写入  生效。",
        );
        ui.add_space(4.0);
        ui.label(format!(
            "当前生效身份：{} / {} / {} / 城市 {} / 锚点({:.6},{:.6})",
            "iOS",
            self.identity.device_name,
            self.identity.os_version,
            self.identity.city,
            self.identity.anchor_lat,
            self.identity.anchor_lon,
        ));
        if self.identity.has_unconfigured_default_location() {
            ui.colored_label(
                theme::warn(),
                "运行前请填写实际城市和定位锚点；默认大连配置不会用于提交",
            );
        }
        if page.saved_flash > 0.0 {
            page.saved_flash -= ui.ctx().input(|i| i.stable_dt);
        }
    }
}

fn location_fields_changed(saved: &HeaderIdentity, pending: &HeaderIdentity) -> bool {
    saved.city.trim() != pending.city.trim()
        || (saved.anchor_lat - pending.anchor_lat).abs() > 1e-9
        || (saved.anchor_lon - pending.anchor_lon).abs() > 1e-9
}

/// 整套随机：uuid v4 设备 ID（大写）；先抽机型，再抽该机型的系统版本。
///
/// 机型与系统必须相关：独立抽样会产出「iPhone 17 Pro + iOS 18.1」这类出厂就不存在的
/// 组合，正是机型/系统交叉校验最容易识别的指纹。
fn randomize(buf: &mut HeaderIdentity) {
    buf.manufacturer.clear();
    buf.platform = "ios".into();
    buf.device_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    buf.app_install_time = HeaderIdentity::fresh_install_time();
    // 整套抽取真机档位：营销名 + 逻辑/物理分辨率（全部 3x，与 UA Scale/3.00 一致）
    let pool = &crate::crypto::header::IOS_DEVICE_POOL;
    let p = pool[(rand::random::<f64>() * pool.len() as f64) as usize % pool.len()];
    buf.device_name = p.0.into();
    buf.logic_pixel = p.1.into();
    buf.physic_pixel = p.2.into();
    let os_pool = crate::crypto::header::os_pool_for_model(&buf.device_name);
    if !os_pool.is_empty() {
        let k = (rand::random::<f64>() * os_pool.len() as f64) as usize % os_pool.len();
        buf.os_version = os_pool[k].into();
    }
    // iOS WHSimulateIDFA：随机 UUID 一经生成即复用
    buf.idfa = uuid::Uuid::new_v4().to_string().to_uppercase();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_changes_are_detected_until_saved() {
        let saved = HeaderIdentity::default();
        let mut pending = saved.clone();
        assert!(!location_fields_changed(&saved, &pending));

        pending.city = "北京市".into();
        assert!(location_fields_changed(&saved, &pending));

        pending = saved.clone();
        pending.anchor_lat += 0.000001;
        assert!(location_fields_changed(&saved, &pending));
    }

    #[test]
    fn random_device_does_not_keep_a_previously_imported_brand() {
        let mut identity = HeaderIdentity {
            manufacturer: "Previously imported brand".into(),
            ..Default::default()
        };
        randomize(&mut identity);
        assert!(
            identity.manufacturer.is_empty(),
            "random identity must not retain a different device's brand"
        );
    }

    /// 机型与系统必须来自同一档位：iPhone 17/Air 只允许 iOS 26 系。
    #[test]
    fn random_device_pairs_model_with_its_own_system_pool() {
        for _ in 0..64 {
            let mut identity = HeaderIdentity::default();
            randomize(&mut identity);
            let pool = crate::crypto::header::os_pool_for_model(&identity.device_name);
            assert!(
                pool.contains(&identity.os_version.as_str()),
                "{} 与 iOS {} 不是同一档位",
                identity.device_name,
                identity.os_version
            );
            let pixels = crate::crypto::header::pixels_for_model(&identity.device_name)
                .expect("random model must come from the device pool");
            assert_eq!(identity.logic_pixel, pixels.0);
            assert_eq!(identity.physic_pixel, pixels.1);
            assert_eq!(identity.platform, "ios");
        }
    }

    /// 留空 IDFA 在保存路径上必须回填派生值，而不是保持空串。
    #[test]
    fn empty_idfa_is_backfilled_with_the_derived_value() {
        let mut identity = HeaderIdentity {
            device_id: "66147D92-F160-4A5E-937B-B8B36FCEE21F".into(),
            idfa: String::new(),
            ..Default::default()
        };
        let derived = identity.effective_idfa();
        if identity.idfa.trim().is_empty() {
            identity.idfa = derived.clone();
        }
        assert!(!identity.idfa.is_empty());
        assert_eq!(identity.idfa, derived);
        assert_eq!(identity.effective_idfa(), derived, "回填后派生值必须不变");
    }
}
