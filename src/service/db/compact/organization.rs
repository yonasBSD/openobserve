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

use config::RwHashMap;
use once_cell::sync::Lazy;

use crate::service::db;

pub static STREAMS: Lazy<RwHashMap<String, RwHashMap<String, i64>>> = Lazy::new(Default::default);

fn mk_key(org_id: &str, module: &str) -> String {
    format!("/compact/organization/{org_id}/{module}")
}

pub async fn get_offset(org_id: &str, module: &str) -> (i64, String) {
    let key = mk_key(org_id, module);
    let mut value = match db::get(&key).await {
        Ok(ret) => String::from_utf8_lossy(&ret).to_string(),
        Err(_) => String::from("0"),
    };
    if value.is_empty() {
        value = String::from("0");
    }
    if value.contains(';') {
        let mut parts = value.split(';');
        let offset: i64 = parts.next().unwrap().parse().unwrap();
        let node = parts.next().unwrap().to_string();
        (offset, node)
    } else {
        (value.parse().unwrap(), String::from(""))
    }
}

pub async fn set_offset(
    org_id: &str,
    module: &str,
    offset: i64,
    node: Option<&str>,
) -> Result<(), anyhow::Error> {
    let key = mk_key(org_id, module);
    let val = if let Some(node) = node {
        format!("{offset};{node}")
    } else {
        offset.to_string()
    };
    Ok(db::put(&key, val.into(), db::NO_NEED_WATCH, None).await?)
}
