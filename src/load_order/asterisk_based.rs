/*
 * This file is part of libloadorder
 *
 * Copyright (C) 2017 Oliver Hamlet
 *
 * libloadorder is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * libloadorder is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with libloadorder. If not, see <http://www.gnu.org/licenses/>.
 */
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};

use unicase::{eq, UniCase};

use super::mutable::{generic_insert_position, hoist_masters, read_plugin_names, MutableLoadOrder};
use super::readable::{ReadableLoadOrder, ReadableLoadOrderBase};
use super::strict_encode;
use super::timestamp_based::save_load_order_using_timestamps;
use super::writable::{
    activate, add, create_parent_dirs, deactivate, remove, set_active_plugins, WritableLoadOrder,
};
use crate::enums::{Error, GameId};
use crate::game_settings::GameSettings;
use crate::plugin::{trim_dot_ghost, Plugin};

#[derive(Clone, Debug)]
pub struct AsteriskBasedLoadOrder {
    game_settings: GameSettings,
    plugins: Vec<Plugin>,
}

impl AsteriskBasedLoadOrder {
    pub fn new(game_settings: GameSettings) -> Self {
        Self {
            game_settings,
            plugins: Vec::new(),
        }
    }

    fn read_from_active_plugins_file(&self) -> Result<Vec<(String, bool)>, Error> {
        if self.ignore_active_plugins_file() {
            Ok(Vec::new())
        } else {
            read_plugin_names(
                self.game_settings().active_plugins_file(),
                owning_plugin_line_mapper,
            )
        }
    }

    fn ignore_active_plugins_file(&self) -> bool {
        // Fallout 4 and Starfield ignore plugins.txt if there are any sTestFile plugins listed in
        // the ini files.
        ignore_active_plugins_file_fallout4(&self.game_settings)
            || ignore_active_plugins_file_starfield(&self.game_settings)
    }
}

impl ReadableLoadOrderBase for AsteriskBasedLoadOrder {
    fn game_settings_base(&self) -> &GameSettings {
        &self.game_settings
    }

    fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }
}

impl MutableLoadOrder for AsteriskBasedLoadOrder {
    fn plugins_mut(&mut self) -> &mut Vec<Plugin> {
        &mut self.plugins
    }

    fn insert_position(&self, plugin: &Plugin) -> Option<usize> {
        if self.game_settings().loads_early(plugin.name()) {
            if self.plugins().is_empty() {
                return None;
            }

            let mut loaded_plugin_count = 0;
            for plugin_name in self.game_settings().early_loading_plugins() {
                if eq(plugin.name(), plugin_name) {
                    return Some(loaded_plugin_count);
                }

                if self.index_of(plugin_name).is_some() {
                    loaded_plugin_count += 1;
                }
            }
        }

        generic_insert_position(self.plugins(), plugin)
    }
}

impl WritableLoadOrder for AsteriskBasedLoadOrder {
    fn game_settings_mut(&mut self) -> &mut GameSettings {
        &mut self.game_settings
    }

    fn load(&mut self) -> Result<(), Error> {
        self.plugins_mut().clear();

        let plugin_tuples = self.read_from_active_plugins_file()?;
        let filenames = self.find_plugins();

        self.load_unique_plugins(plugin_tuples, filenames);
        hoist_masters(&mut self.plugins)?;

        self.add_implicitly_active_plugins()?;

        Ok(())
    }

    fn save(&mut self) -> Result<(), Error> {
        let path = self.game_settings().active_plugins_file();
        create_parent_dirs(path)?;

        let file = File::create(path).map_err(|e| Error::IoError(path.clone(), e))?;
        let mut writer = BufWriter::new(file);
        for plugin in self.plugins() {
            if self.game_settings().loads_early(plugin.name()) {
                // Skip early loading plugins, but not implicitly active plugins
                // as they may need load order positions defined.
                continue;
            }

            if plugin.is_active() {
                write!(writer, "*").map_err(|e| Error::IoError(path.clone(), e))?;
            }
            writer
                .write_all(&strict_encode(plugin.name())?)
                .map_err(|e| Error::IoError(path.clone(), e))?;
            writeln!(writer).map_err(|e| Error::IoError(path.clone(), e))?;
        }

        if self.ignore_active_plugins_file() {
            // If the active plugins file is being ignored there's no harm in
            // writing to it, but it won't actually have any impact on the load
            // order used by the game. In that case, the only way to set the
            // load order is to modify plugin timestamps, so do that.
            save_load_order_using_timestamps(self)?;
        }

        Ok(())
    }

    fn add(&mut self, plugin_name: &str) -> Result<usize, Error> {
        add(self, plugin_name)
    }

    fn remove(&mut self, plugin_name: &str) -> Result<(), Error> {
        remove(self, plugin_name)
    }

    fn set_load_order(&mut self, plugin_names: &[&str]) -> Result<(), Error> {
        let game_master_file = self.game_settings().master_file();

        let is_game_master_first = plugin_names
            .first()
            .map(|name| eq(*name, game_master_file))
            .unwrap_or(false);
        if !is_game_master_first {
            return Err(Error::GameMasterMustLoadFirst(game_master_file.to_string()));
        }

        // Check that all early loading plugins that are present load in
        // their hardcoded order.
        let mut missing_plugins_count = 0;
        for (i, plugin_name) in self
            .game_settings()
            .early_loading_plugins()
            .iter()
            .enumerate()
        {
            match plugin_names.iter().position(|n| eq(*n, plugin_name)) {
                Some(pos) => {
                    let expected_pos = i - missing_plugins_count;
                    if pos != expected_pos {
                        return Err(Error::InvalidEarlyLoadingPluginPosition {
                            name: plugin_name.clone(),
                            pos,
                            expected_pos,
                        });
                    }
                }
                None => missing_plugins_count += 1,
            }
        }

        self.replace_plugins(plugin_names)
    }

    fn set_plugin_index(&mut self, plugin_name: &str, position: usize) -> Result<usize, Error> {
        let game_master_file = self.game_settings().master_file();

        if position != 0 && !self.plugins().is_empty() && eq(plugin_name, game_master_file) {
            return Err(Error::GameMasterMustLoadFirst(game_master_file.to_string()));
        }
        if position == 0 && !eq(plugin_name, game_master_file) {
            return Err(Error::GameMasterMustLoadFirst(game_master_file.to_string()));
        }

        self.move_or_insert_plugin_with_index(plugin_name, position)
    }

    fn is_self_consistent(&self) -> Result<bool, Error> {
        Ok(true)
    }

    /// An asterisk-based load order can be ambiguous if there are installed
    /// plugins that are not implicitly active and not listed in plugins.txt.
    fn is_ambiguous(&self) -> Result<bool, Error> {
        let mut set = HashSet::new();

        // Read plugins from the active plugins file. A set of plugin names is
        // more useful than the returned vec, so insert into the set during the
        // line mapping and then discard the line.
        if !self.ignore_active_plugins_file() {
            read_plugin_names(self.game_settings().active_plugins_file(), |line| {
                plugin_line_mapper(line).and_then::<(), _>(|(name, _)| {
                    set.insert(UniCase::new(trim_dot_ghost(name).to_string()));
                    None
                })
            })?;
        }

        // All implicitly active plugins have a defined load order position,
        // even if they're not in plugins.txt or the early loaders.
        // Plugins that are active but not implicitly active, and plugins that
        // are inactive, only have a load order position if they're listed in
        // plugins.txt, so check that they're all listed.
        let plugins_listed = self
            .plugins
            .iter()
            .filter(|plugin| !self.game_settings.is_implicitly_active(plugin.name()))
            .all(|plugin| set.contains(&UniCase::new(plugin.name().to_string())));

        Ok(!plugins_listed)
    }

    fn activate(&mut self, plugin_name: &str) -> Result<(), Error> {
        activate(self, plugin_name)
    }

    fn deactivate(&mut self, plugin_name: &str) -> Result<(), Error> {
        deactivate(self, plugin_name)
    }

    fn set_active_plugins(&mut self, active_plugin_names: &[&str]) -> Result<(), Error> {
        set_active_plugins(self, active_plugin_names)
    }
}

fn plugin_line_mapper(line: &str) -> Option<(&str, bool)> {
    if line.is_empty() || line.starts_with('#') {
        None
    } else if line.as_bytes()[0] == b'*' {
        Some((&line[1..], true))
    } else {
        Some((line, false))
    }
}

fn owning_plugin_line_mapper(line: &str) -> Option<(String, bool)> {
    plugin_line_mapper(line).map(|(name, active)| (name.to_owned(), active))
}

fn ignore_active_plugins_file_fallout4(game_settings: &GameSettings) -> bool {
    // The implicitly active plugins are the early loading plugins plus test file plugins.
    matches!(game_settings.id(), GameId::Fallout4 | GameId::Fallout4VR)
        && game_settings.implicitly_active_plugins().len()
            > game_settings.early_loading_plugins().len()
}

fn ignore_active_plugins_file_starfield(game_settings: &GameSettings) -> bool {
    // The implicitly active plugins are the early loading plugins plus test file plugins plus
    // BlueprintShips-Starfield.esm.
    game_settings.id() == GameId::Starfield
        && game_settings.implicitly_active_plugins().len()
            > game_settings.early_loading_plugins().len() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::enums::GameId;
    use crate::load_order::tests::*;
    use crate::tests::{copy_to_dir, copy_to_test_dir};
    use std::fs::{create_dir_all, remove_dir_all, File};
    use std::io;
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use tempfile::tempdir;

    fn prepare(game_id: GameId, game_dir: &Path) -> AsteriskBasedLoadOrder {
        let (game_settings, plugins) = mock_game_files(game_id, game_dir);
        AsteriskBasedLoadOrder {
            game_settings,
            plugins,
        }
    }

    fn prepare_bulk_plugins(game_settings: &GameSettings) -> Vec<String> {
        let mut plugins: Vec<String> = vec![game_settings.master_file().to_string()];
        plugins.extend((0..260).map(|i| format!("Blank{}.esm", i)));
        plugins.extend((0..5000).map(|i| format!("Blank{}.esl", i)));

        for plugin in &plugins {
            copy_to_test_dir("Blank - Different.esm", &plugin, game_settings);
        }

        write_active_plugins_file(game_settings, &plugins);

        plugins
    }

    #[test]
    fn ignore_active_plugins_file_should_be_true_for_fallout4_when_test_files_are_configured() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Fallout4.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        assert!(load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_false_for_fallout4_when_test_files_are_not_configured()
    {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        assert!(!load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_true_for_fallout4vr_when_test_files_are_configured() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Fallout4VR.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let load_order = prepare(GameId::Fallout4VR, &tmp_dir.path());

        assert!(load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_false_for_fallout4vr_when_test_files_are_not_configured(
    ) {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Fallout4VR, &tmp_dir.path());

        assert!(!load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_true_for_starfield_when_test_files_are_configured() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/StarfieldCustom.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let load_order = prepare(GameId::Starfield, &tmp_dir.path());

        assert!(load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_false_for_starfield_when_test_files_are_not_configured()
    {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Starfield, &tmp_dir.path());

        assert!(!load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_false_for_skyrimse() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Skyrim.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=a").unwrap();

        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(!load_order.ignore_active_plugins_file());
    }

    #[test]
    fn ignore_active_plugins_file_should_be_false_for_skyrimvr() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/SkyrimVR.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=a").unwrap();

        let load_order = prepare(GameId::SkyrimVR, &tmp_dir.path());

        assert!(!load_order.ignore_active_plugins_file());
    }

    #[test]
    fn insert_position_should_return_none_for_the_game_master_if_no_plugins_are_loaded() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        load_order.plugins_mut().clear();

        let plugin = Plugin::new("Skyrim.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert!(position.is_none());
    }

    #[test]
    fn insert_position_should_return_the_hardcoded_index_of_an_early_loading_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        load_order.plugins_mut().insert(1, plugin);

        copy_to_test_dir("Blank.esm", "HearthFires.esm", &load_order.game_settings());
        let plugin = Plugin::new("HearthFires.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(1, position.unwrap());
    }

    #[test]
    fn insert_position_should_not_treat_all_implicitly_active_plugins_as_early_loading_plugins() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Skyrim.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esm").unwrap();

        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir(
            "Blank.esm",
            "Blank - Different.esm",
            &load_order.game_settings(),
        );
        let plugin = Plugin::new("Blank - Different.esm", &load_order.game_settings()).unwrap();
        load_order.plugins_mut().insert(1, plugin);

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(2, position.unwrap());
    }

    #[test]
    fn insert_position_should_not_count_installed_unloaded_early_loading_plugins() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "Update.esm", &load_order.game_settings());
        copy_to_test_dir("Blank.esm", "HearthFires.esm", &load_order.game_settings());
        let plugin = Plugin::new("HearthFires.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(1, position.unwrap());
    }

    #[test]
    fn insert_position_should_return_none_if_given_a_non_master_plugin() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugin =
            Plugin::new("Blank - Master Dependent.esp", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(None, position);
    }

    #[test]
    fn insert_position_should_return_the_first_non_master_plugin_index_if_given_a_master_plugin() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(1, position.unwrap());
    }

    #[test]
    fn insert_position_should_return_none_if_no_non_masters_are_present() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        // Remove non-master plugins from the load order.
        load_order.plugins_mut().retain(|p| p.is_master_file());

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(None, position);
    }

    #[test]
    fn insert_position_should_return_the_first_non_master_index_if_given_a_light_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "Blank.esl", load_order.game_settings());
        let plugin = Plugin::new("Blank.esl", &load_order.game_settings()).unwrap();

        load_order.plugins_mut().insert(1, plugin);

        let position = load_order.insert_position(&load_order.plugins()[1]);

        assert_eq!(2, position.unwrap());

        copy_to_test_dir(
            "Blank.esp",
            "Blank - Different.esl",
            load_order.game_settings(),
        );
        let plugin = Plugin::new("Blank - Different.esl", &load_order.game_settings()).unwrap();

        let position = load_order.insert_position(&plugin);

        assert_eq!(2, position.unwrap());
    }

    #[test]
    fn load_should_reload_existing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(!load_order.plugins()[1].is_master_file());
        copy_to_test_dir("Blank.esm", "Blank.esp", &load_order.game_settings());
        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank.esp");
        set_file_timestamps(&plugin_path, 0);

        load_order.load().unwrap();

        assert!(load_order.plugins()[1].is_master_file());
    }

    #[test]
    fn load_should_remove_plugins_that_fail_to_load() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(load_order.index_of("Blank.esp").is_some());
        assert!(load_order.index_of("Blank - Different.esp").is_some());

        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank.esp");
        File::create(&plugin_path).unwrap();
        set_file_timestamps(&plugin_path, 0);

        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank - Different.esp");
        File::create(&plugin_path).unwrap();
        set_file_timestamps(&plugin_path, 0);

        load_order.load().unwrap();
        assert!(load_order.index_of("Blank.esp").is_none());
        assert!(load_order.index_of("Blank - Different.esp").is_none());
    }

    #[test]
    fn load_should_get_load_order_from_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["Blank.esp", "Blank - Master Dependent.esp"],
        );

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_hoist_non_masters_that_masters_depend_on_to_load_before_their_dependents() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        // .esm plugins are loaded as ESMs, .esl plugins are loaded as ESMs and
        // ESLs, ignoring their actual flags, so only worth testing a .esp that
        // has the ESM flag set that has another (normal) .esp as a master.

        let plugins_dir = &load_order.game_settings().plugins_directory();
        copy_to_test_dir(
            "Blank - Plugin Dependent.esp",
            "Blank - Plugin Dependent.esp",
            load_order.game_settings(),
        );
        set_master_flag(&plugins_dir.join("Blank - Plugin Dependent.esp"), true).unwrap();

        let expected_filenames = vec![
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
            "Blank.esp",
            "Skyrim.esm",
            "Blank - Plugin Dependent.esp",
            "Blank.esm",
        ];
        write_active_plugins_file(load_order.game_settings(), &expected_filenames);

        load_order.load().unwrap();

        let expected_filenames = vec![
            "Skyrim.esm",
            "Blank.esp",
            "Blank - Plugin Dependent.esp",
            "Blank.esm",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_decode_active_plugins_file_from_windows_1252() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blàñk.esp",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_handle_crlf_and_lf_in_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm\r"]);

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blàñk.esp",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_ignore_active_plugins_file_lines_starting_with_a_hash() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["#Blank.esp", "Blàñk.esp", "Blank.esm"],
        );

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blàñk.esp",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_ignore_plugins_in_active_plugins_file_that_are_not_installed() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["Blàñk.esp", "Blank.esm", "missing.esp"],
        );

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blàñk.esp",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_add_missing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(load_order.index_of("Blank.esm").is_none());
        assert!(load_order
            .index_of("Blank - Master Dependent.esp")
            .is_none());
        assert!(load_order.index_of("Blàñk.esp").is_none());

        load_order.load().unwrap();

        assert!(load_order.index_of("Blank.esm").is_some());
        assert!(load_order
            .index_of("Blank - Master Dependent.esp")
            .is_some());
        assert!(load_order.index_of("Blàñk.esp").is_some());
    }

    #[test]
    fn load_should_recognise_light_master_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "ccTest.esl", &load_order.game_settings());

        load_order.load().unwrap();

        assert!(load_order.plugin_names().contains(&"ccTest.esl"));
    }

    #[test]
    fn load_should_add_missing_early_loading_plugins_in_their_hardcoded_positions() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "Update.esm", &load_order.game_settings());
        load_order.load().unwrap();
        assert_eq!(Some(1), load_order.index_of("Update.esm"));
        assert!(load_order.is_active("Update.esm"));
    }

    #[test]
    fn load_should_empty_the_load_order_if_the_plugins_directory_does_not_exist() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());
        tmp_dir.close().unwrap();

        load_order.load().unwrap();

        assert!(load_order.plugins().is_empty());
    }

    #[test]
    fn load_should_load_plugin_states_from_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.load().unwrap();
        let expected_filenames = vec!["Skyrim.esm", "Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_succeed_when_active_plugins_file_is_missing() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(load_order.load().is_ok());
        assert_eq!(1, load_order.active_plugin_names().len());
    }

    #[test]
    fn load_should_not_duplicate_a_plugin_that_has_a_ghosted_duplicate() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        use std::fs::copy;

        copy(
            load_order
                .game_settings()
                .plugins_directory()
                .join("Blank.esm"),
            load_order
                .game_settings()
                .plugins_directory()
                .join("Blank.esm.ghost"),
        )
        .unwrap();

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
            "Blàñk.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_not_move_light_master_esp_files_before_non_masters() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esl", "Blank.esl.esp", &load_order.game_settings());

        load_order.load().unwrap();

        let expected_filenames = vec![
            load_order.game_settings().master_file(),
            "Blank.esm",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
            "Blàñk.esp",
            "Blank.esl.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_add_plugins_in_additional_plugins_directory_before_those_in_main_plugins_directory(
    ) {
        let tmp_dir = tempdir().unwrap();
        let game_path = tmp_dir.path().join("Fallout 4/Content");
        create_dir_all(&game_path).unwrap();

        File::create(game_path.join("appxmanifest.xml")).unwrap();

        let mut load_order = prepare(GameId::Fallout4, &game_path);

        copy_to_test_dir("Blank.esm", "Blank.esm", &load_order.game_settings());

        let dlc_path = tmp_dir
            .path()
            .join("Fallout 4- Far Harbor (PC)/Content/Data");
        create_dir_all(&dlc_path).unwrap();
        copy_to_dir("Blank.esm", &dlc_path, "DLCCoast.esm", GameId::Fallout4);
        copy_to_dir("Blank.esp", &dlc_path, "Blank DLC.esp", GameId::Fallout4);

        load_order.load().unwrap();

        let expected_filenames = vec![
            "Fallout4.esm",
            "DLCCoast.esm",
            "Blank.esm",
            "Blank.esp",
            "Blank - Different.esp",
            "Blank - Master Dependent.esp",
            "Blàñk.esp",
            "Blank DLC.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_ignore_active_plugins_file_for_fallout4_when_test_files_are_configured() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Fallout4.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let mut load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["Blank.esp", "Blank - Master Dependent.esp"],
        );

        load_order.load().unwrap();

        assert_eq!(
            vec!["Fallout4.esm", "Blank.esp"],
            load_order.active_plugin_names()
        );
    }

    #[test]
    fn save_should_create_active_plugins_file_parent_directory_if_it_does_not_exist() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        remove_dir_all(
            load_order
                .game_settings()
                .active_plugins_file()
                .parent()
                .unwrap(),
        )
        .unwrap();

        load_order.save().unwrap();

        assert!(load_order
            .game_settings()
            .active_plugins_file()
            .parent()
            .unwrap()
            .exists());
    }

    #[test]
    fn save_should_write_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        load_order.save().unwrap();

        load_order.load().unwrap();
        assert_eq!(
            vec!["Skyrim.esm", "Blank.esp"],
            load_order.active_plugin_names()
        );
    }

    #[test]
    fn save_should_write_unghosted_plugin_names() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir(
            "Blank - Different.esm",
            "ghosted.esm.ghost",
            &load_order.game_settings(),
        );
        let plugin = Plugin::new("ghosted.esm.ghost", &load_order.game_settings()).unwrap();
        load_order.plugins_mut().push(plugin);

        load_order.save().unwrap();

        let reader =
            BufReader::new(File::open(load_order.game_settings().active_plugins_file()).unwrap());

        let lines = reader
            .lines()
            .collect::<Result<Vec<String>, io::Error>>()
            .unwrap();

        assert_eq!(
            vec!["*Blank.esp", "Blank - Different.esp", "ghosted.esm"],
            lines
        );
    }

    #[test]
    fn save_should_error_if_a_plugin_filename_cannot_be_encoded_in_windows_1252() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let filename = "Bl\u{0227}nk.esm";
        copy_to_test_dir(
            "Blank - Different.esm",
            filename,
            &load_order.game_settings(),
        );
        let plugin = Plugin::new(filename, &load_order.game_settings()).unwrap();
        load_order.plugins_mut().push(plugin);

        match load_order.save().unwrap_err() {
            Error::EncodeError(s) => assert_eq!("Blȧnk.esm", s),
            e => panic!("Expected encode error, got {:?}", e),
        };
    }

    #[test]
    fn save_should_omit_early_loading_plugins_from_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "HearthFires.esm", &load_order.game_settings());
        let plugin = Plugin::new("HearthFires.esm", &load_order.game_settings()).unwrap();
        load_order.plugins_mut().push(plugin);

        load_order.save().unwrap();

        let reader =
            BufReader::new(File::open(load_order.game_settings().active_plugins_file()).unwrap());

        let lines = reader
            .lines()
            .collect::<Result<Vec<String>, io::Error>>()
            .unwrap();

        assert_eq!(vec!["*Blank.esp", "Blank - Different.esp"], lines);
    }

    #[test]
    fn save_should_not_omit_implicitly_active_plugins_that_do_not_load_early() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Skyrim.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank - Different.esp").unwrap();

        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        load_order.load().unwrap();

        load_order.save().unwrap();

        let content = std::fs::read(load_order.game_settings().active_plugins_file()).unwrap();
        let content = encoding_rs::WINDOWS_1252.decode(&content).0;

        let lines = content.lines().collect::<Vec<&str>>();

        assert_eq!(
            vec![
                "Blank.esm",
                "Blank.esp",
                "*Blank - Different.esp",
                "Blank - Master Dependent.esp",
                "Blàñk.esp",
            ],
            lines
        );
    }

    #[test]
    fn save_should_modify_plugin_timestamps_if_active_plugins_file_is_ignored() {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Fallout4.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let mut load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        let filename = "Blank.esp";
        let plugin_path = load_order.game_settings.plugins_directory().join(filename);

        let original_timestamp = plugin_path.metadata().unwrap().modified().unwrap();

        assert_eq!(1, load_order.index_of(filename).unwrap());
        load_order.set_plugin_index(filename, 2).unwrap();

        load_order.save().unwrap();

        let new_timestamp = plugin_path.metadata().unwrap().modified().unwrap();

        assert_eq!(
            original_timestamp + std::time::Duration::from_secs(60),
            new_timestamp
        );
    }

    #[test]
    fn save_should_not_modify_plugin_timestamps_if_active_plugins_file_is_not_ignored() {
        let tmp_dir = tempdir().unwrap();

        let mut load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        let filename = "Blank.esp";
        let plugin_path = load_order.game_settings.plugins_directory().join(filename);

        let original_timestamp = plugin_path.metadata().unwrap().modified().unwrap();

        assert_eq!(1, load_order.index_of(filename).unwrap());
        load_order.set_plugin_index(filename, 2).unwrap();

        load_order.save().unwrap();

        let new_timestamp = plugin_path.metadata().unwrap().modified().unwrap();

        assert_eq!(original_timestamp, new_timestamp);
    }

    #[test]
    fn set_load_order_should_error_if_given_an_empty_list() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        let filenames = vec![];
        assert!(load_order.set_load_order(&filenames).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_error_if_the_first_element_given_is_not_the_game_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        let filenames = vec!["Blank.esp"];
        assert!(load_order.set_load_order(&filenames).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_error_if_an_early_loading_plugin_loads_after_another_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "Update.esm", &load_order.game_settings());

        let filenames = vec![
            "Skyrim.esm",
            "Blank.esm",
            "Update.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        match load_order.set_load_order(&filenames).unwrap_err() {
            Error::InvalidEarlyLoadingPluginPosition {
                name,
                pos,
                expected_pos,
            } => {
                assert_eq!("Update.esm", name);
                assert_eq!(2, pos);
                assert_eq!(1, expected_pos);
            }
            e => panic!("Wrong error type: {:?}", e),
        }
    }

    #[test]
    fn set_load_order_should_not_error_if_a_non_early_loading_implicitly_active_plugin_loads_after_another_plugin(
    ) {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Skyrim.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank - Different.esp").unwrap();

        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let filenames = vec![
            "Skyrim.esm",
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert!(load_order.set_load_order(&filenames).is_ok());
    }

    #[test]
    fn set_load_order_should_not_error_if_an_early_loading_plugin_is_missing() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir("Blank.esm", "Dragonborn.esm", &load_order.game_settings());

        let filenames = vec![
            "Skyrim.esm",
            "Dragonborn.esm",
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert!(load_order.set_load_order(&filenames).is_ok());
    }

    #[test]
    fn set_load_order_should_not_distinguish_between_ghosted_and_unghosted_filenames() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        copy_to_test_dir(
            "Blank - Different.esm",
            "ghosted.esm.ghost",
            &load_order.game_settings(),
        );

        let filenames = vec![
            "Skyrim.esm",
            "Blank.esm",
            "ghosted.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert!(load_order.set_load_order(&filenames).is_ok());
    }

    #[test]
    fn set_load_order_should_not_insert_missing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let filenames = vec![
            "Skyrim.esm",
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
        ];
        load_order.set_load_order(&filenames).unwrap();

        assert_eq!(filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_not_lose_active_state_of_existing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let filenames = vec![
            "Skyrim.esm",
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
        ];
        load_order.set_load_order(&filenames).unwrap();

        assert!(load_order.is_active("Blank.esp"));
    }

    #[test]
    fn set_plugin_index_should_error_if_setting_the_game_master_index_to_non_zero_in_bounds() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("Skyrim.esm", 1).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_error_if_setting_a_zero_index_for_a_non_game_master_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("Blank.esm", 0).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_insert_a_new_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let num_plugins = load_order.plugins().len();
        assert_eq!(1, load_order.set_plugin_index("Blank.esm", 1).unwrap());
        assert_eq!(1, load_order.index_of("Blank.esm").unwrap());
        assert_eq!(num_plugins + 1, load_order.plugins().len());
    }

    #[test]
    fn is_self_consistent_should_return_true() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(load_order.is_self_consistent().unwrap());
    }

    #[test]
    fn is_ambiguous_should_return_false_if_all_loaded_plugins_are_listed_in_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let loaded_plugin_names: Vec<&str> = load_order
            .plugins
            .iter()
            .map(|plugin| plugin.name())
            .collect();
        write_active_plugins_file(load_order.game_settings(), &loaded_plugin_names);

        assert!(!load_order.is_ambiguous().unwrap());
    }

    #[test]
    fn is_ambiguous_should_ignore_plugins_that_are_listed_in_active_plugins_file_but_not_loaded() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        assert!(load_order.index_of("missing.esp").is_none());

        let mut loaded_plugin_names: Vec<&str> = load_order
            .plugins
            .iter()
            .map(|plugin| plugin.name())
            .collect();
        loaded_plugin_names.push("missing.esp");

        write_active_plugins_file(load_order.game_settings(), &loaded_plugin_names);

        assert!(!load_order.is_ambiguous().unwrap());
    }

    #[test]
    fn is_ambiguous_should_ignore_loaded_implicitly_active_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Starfield, &tmp_dir.path());

        let loaded_plugin_names: Vec<&str> = load_order
            .plugins
            .iter()
            .map(|plugin| plugin.name())
            .collect();

        write_active_plugins_file(load_order.game_settings(), &loaded_plugin_names);

        copy_to_test_dir(
            "Blank.esm",
            "BlueprintShips-Starfield.esm",
            &load_order.game_settings(),
        );
        let plugin =
            Plugin::new("BlueprintShips-Starfield.esm", &load_order.game_settings()).unwrap();
        load_order.plugins_mut().push(plugin);

        assert!(!load_order.is_ambiguous().unwrap());
    }

    #[test]
    fn is_ambiguous_should_return_true_if_there_are_loaded_plugins_not_in_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let mut loaded_plugin_names: Vec<&str> = load_order
            .plugins
            .iter()
            .map(|plugin| plugin.name())
            .collect();

        loaded_plugin_names.pop();

        write_active_plugins_file(load_order.game_settings(), &loaded_plugin_names);

        assert!(load_order.is_ambiguous().unwrap());
    }

    #[test]
    fn is_ambiguous_should_ignore_the_active_plugins_file_for_fallout4_when_test_files_are_configured(
    ) {
        let tmp_dir = tempdir().unwrap();

        let ini_path = tmp_dir.path().join("my games/Fallout4.ini");
        create_parent_dirs(&ini_path).unwrap();
        std::fs::write(&ini_path, "[General]\nsTestFile1=Blank.esp").unwrap();

        let load_order = prepare(GameId::Fallout4, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &load_order.plugin_names());

        assert!(load_order.is_ambiguous().unwrap());
    }

    #[test]
    fn activate_should_check_normal_plugins_and_light_masters_active_limits_separately() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugins = prepare_bulk_plugins(load_order.game_settings());

        let mut plugin_refs: Vec<&str> = plugins[..254].iter().map(AsRef::as_ref).collect();
        plugin_refs.extend(plugins[261..4356].iter().map(|s| s.as_str()));

        load_order.load().unwrap();
        assert!(load_order.set_active_plugins(&plugin_refs).is_ok());

        let i = 4356;
        assert!(load_order.activate(&plugins[i]).is_ok());
        assert!(load_order.is_active(&plugins[i]));

        let i = 254;
        assert!(load_order.activate(&plugins[i]).is_ok());
        assert!(load_order.is_active(&plugins[i]));

        let i = 256;
        assert!(load_order.activate(&plugins[i]).is_err());
        assert!(!load_order.is_active(&plugins[i]));

        let i = 4357;
        assert!(load_order.activate(&plugins[i]).is_err());
        assert!(!load_order.is_active(&plugins[i]));
    }

    #[test]
    fn set_active_plugins_should_count_light_masters_and_normal_plugins_separately() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugins = prepare_bulk_plugins(load_order.game_settings());

        let mut plugin_refs: Vec<&str> = plugins[..255].iter().map(AsRef::as_ref).collect();
        plugin_refs.extend(plugins[261..4357].iter().map(|s| s.as_str()));

        load_order.load().unwrap();
        assert!(load_order.set_active_plugins(&plugin_refs).is_ok());
        assert_eq!(4351, load_order.active_plugin_names().len());
    }

    #[test]
    fn set_active_plugins_should_error_if_given_more_than_4096_light_masters() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::SkyrimSE, &tmp_dir.path());

        let plugins = prepare_bulk_plugins(load_order.game_settings());

        let mut plugin_refs: Vec<&str> = plugins[..255].iter().map(AsRef::as_ref).collect();
        plugin_refs.extend(plugins[261..4358].iter().map(|s| s.as_str()));

        assert!(load_order.set_active_plugins(&plugin_refs).is_err());
        assert_eq!(1, load_order.active_plugin_names().len());
    }
}
