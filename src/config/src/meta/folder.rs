// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Folder {
    pub folder_id: String,
    pub name: String,
    pub description: String,
}

/// Indicates the type of data that the folder can contain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub enum FolderType {
    #[default]
    Dashboards,
    Alerts,
    Reports,
}

pub const DEFAULT_FOLDER: &str = "default";
