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

use std::fmt::Debug;

use actix_web::{Error, FromRequest, HttpRequest, dev::Payload};
use base64::Engine;
use config::{
    meta::user::UserRole,
    utils::{hash::get_passcode_hash, json},
};
use futures::future::{Ready, ready};
use once_cell::sync::Lazy;
use regex::Regex;
#[cfg(feature = "enterprise")]
use {
    crate::{
        common::{infra::config::USER_SESSIONS, meta::ingestion::INGESTION_EP},
        service::users::get_user,
    },
    jsonwebtoken::TokenData,
    o2_dex::service::auth::get_dex_jwks,
    o2_openfga::config::get_config as get_openfga_config,
    o2_openfga::meta::mapping::OFGA_MODELS,
    serde_json::Value,
    std::{collections::HashMap, str::FromStr},
};

use crate::common::{
    infra::config::{ORG_USERS, PASSWORD_HASH},
    meta::{
        authz::Authz,
        organization::DEFAULT_ORG,
        user::{AuthTokens, UserOrgRole},
    },
};

pub const V2_API_PREFIX: &str = "v2";

pub static RE_OFGA_UNSUPPORTED_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[:#?\s'"%&]+"#).unwrap());
static RE_SPACE_AROUND: Lazy<Regex> = Lazy::new(|| {
    let char_pattern = r#"[^a-zA-Z0-9:#?'"&%\s]"#;
    let pattern = format!(r"(\s+{char_pattern}\s+)|(\s+{char_pattern})|({char_pattern}\s+)");
    Regex::new(&pattern).unwrap()
});

pub static EMAIL_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^([a-zA-Z0-9_+]([a-zA-Z0-9_+.-]*[a-zA-Z0-9_+])?)@([a-zA-Z0-9]+([\-\.]{1}[a-zA-Z0-9]+)*\.[a-zA-Z]{2,6})",
    )
    .unwrap()
});

pub fn is_valid_email(email: &str) -> bool {
    EMAIL_REGEX.is_match(email)
}

pub fn into_ofga_supported_format(name: &str) -> String {
    // remove spaces around special characters
    let result = RE_SPACE_AROUND.replace_all(name, |caps: &regex::Captures| {
        caps.iter()
            .find_map(|m| m)
            .map(|m| m.as_str().trim())
            .unwrap_or("")
            .to_string()
    });
    RE_OFGA_UNSUPPORTED_NAME
        .replace_all(&result, "_")
        .to_string()
}

pub fn is_ofga_unsupported(name: &str) -> bool {
    RE_OFGA_UNSUPPORTED_NAME.is_match(name)
}

pub(crate) fn get_hash(pass: &str, salt: &str) -> String {
    let key = format!("{pass}{salt}");
    let hash = PASSWORD_HASH.get(&key);
    match hash {
        Some(ret_hash) => ret_hash.value().to_string(),
        None => {
            let password_hash = get_passcode_hash(pass, salt);
            PASSWORD_HASH.insert(key, password_hash.clone());
            password_hash
        }
    }
}

pub(crate) fn is_root_user(user_id: &str) -> bool {
    match ORG_USERS.get(&format!("{DEFAULT_ORG}/{user_id}")) {
        Some(user) => user.role.eq(&UserRole::Root),
        None => false,
    }
}

#[cfg(feature = "enterprise")]
pub async fn save_org_tuples(org_id: &str) {
    use o2_openfga::config::get_config as get_openfga_config;

    if get_openfga_config().enabled {
        o2_openfga::authorizer::authz::save_org_tuples(org_id).await
    }
}

#[cfg(not(feature = "enterprise"))]
pub async fn save_org_tuples(_org_id: &str) {}

#[cfg(feature = "enterprise")]
pub async fn delete_org_tuples(org_id: &str) {
    use o2_openfga::config::get_config as get_openfga_config;

    if get_openfga_config().enabled {
        o2_openfga::authorizer::authz::delete_org_tuples(org_id).await
    }
}

#[cfg(not(feature = "enterprise"))]
pub async fn delete_org_tuples(_org_id: &str) {}

#[cfg(feature = "enterprise")]
pub fn get_role(role: &UserOrgRole) -> UserRole {
    use std::str::FromStr;

    let role = o2_openfga::authorizer::roles::get_role(format!("{}", role.base_role));
    UserRole::from_str(&role).unwrap()
}

#[cfg(not(feature = "enterprise"))]
pub fn get_role(_role: &UserOrgRole) -> UserRole {
    UserRole::Admin
}

#[cfg(feature = "enterprise")]
pub async fn set_ownership(org_id: &str, obj_type: &str, obj: Authz) {
    if get_openfga_config().enabled {
        use o2_openfga::{authorizer, meta::mapping::OFGA_MODELS};

        let obj_str = format!("{}:{}", OFGA_MODELS.get(obj_type).unwrap().key, obj.obj_id);

        let parent_type = if obj.parent_type.is_empty() {
            ""
        } else {
            OFGA_MODELS.get(obj.parent_type.as_str()).unwrap().key
        };

        // Default folder is already created in case of new org, this handles the case for old org
        if obj_type.eq("folders")
            && authorizer::authz::check_folder_exists(org_id, &obj.obj_id).await
        {
            // If the folder tuples are missing, it automatically creates them
            // So we can return here
            log::debug!(
                "folder tuples already exists for org: {org_id}; folder: {}",
                &obj.obj_id
            );
            return;
        } else if obj.parent_type.eq("folders") {
            log::debug!("checking parent folder tuples for folder: {}", &obj.parent);
            // In case of dashboard, we need to check if the tuples for its folder exist
            // If not, the below function creates the proper tuples for the folder
            authorizer::authz::check_folder_exists(org_id, &obj.parent).await;
        }
        authorizer::authz::set_ownership(org_id, &obj_str, &obj.parent, parent_type).await;
    }
}
#[cfg(not(feature = "enterprise"))]
pub async fn set_ownership(_org_id: &str, _obj_type: &str, _obj: Authz) {}

#[cfg(feature = "enterprise")]
pub async fn remove_ownership(org_id: &str, obj_type: &str, obj: Authz) {
    if get_openfga_config().enabled {
        use o2_openfga::{authorizer, meta::mapping::OFGA_MODELS};
        let obj_str = format!("{}:{}", OFGA_MODELS.get(obj_type).unwrap().key, obj.obj_id);

        let parent_type = if obj.parent_type.is_empty() {
            ""
        } else {
            OFGA_MODELS.get(obj.parent_type.as_str()).unwrap().key
        };

        authorizer::authz::remove_ownership(org_id, &obj_str, &obj.parent, parent_type).await;
    }
}
#[cfg(not(feature = "enterprise"))]
pub async fn remove_ownership(_org_id: &str, _obj_type: &str, _obj: Authz) {}

pub struct UserEmail {
    pub user_id: String,
}

impl FromRequest for UserEmail {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if let Some(auth_header) = req.headers().get("user_id")
            && let Ok(user_str) = auth_header.to_str()
        {
            return ready(Ok(UserEmail {
                user_id: user_str.to_lowercase(),
            }));
        }
        ready(Err(actix_web::error::ErrorUnauthorized("No user found")))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct AuthExtractor {
    pub auth: String,
    pub method: String,
    pub o2_type: String,
    pub org_id: String,
    pub bypass_check: bool,
    pub parent_id: String,
}

impl FromRequest for AuthExtractor {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    #[cfg(feature = "enterprise")]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        use actix_web::web;
        use config::{get_config, meta::stream::StreamType};
        use hashbrown::HashMap;
        use o2_openfga::meta::mapping::OFGA_MODELS;

        use crate::common::utils::http::{get_folder, get_stream_type_from_request};

        let start = std::time::Instant::now();

        let query = web::Query::<HashMap<String, String>>::from_query(req.query_string()).unwrap();
        let stream_type = get_stream_type_from_request(&query);

        let folder = get_folder(&query);

        let mut method = req.method().to_string();
        let local_path = req.path().to_string();
        let path = match local_path
            .strip_prefix(format!("{}/api/", config::get_config().common.base_uri).as_str())
        {
            Some(path) => path,
            None => &local_path,
        };

        let path_columns = path.split('/').collect::<Vec<&str>>();
        let url_len = path_columns.len();
        let org_id = if url_len > 1 && path_columns[0].eq(V2_API_PREFIX) {
            path_columns[1].to_string()
        } else {
            path_columns[0].to_string()
        };

        // This is case for ingestion endpoints where we need to check
        // permissions on the stream
        if method.eq("POST") && INGESTION_EP.contains(&path_columns[url_len - 1]) {
            if let Some(auth_header) = req.headers().get("Authorization")
                && let Ok(auth_str) = auth_header.to_str()
            {
                return ready(Ok(AuthExtractor {
                    auth: auth_str.to_owned(),
                    method,
                    o2_type: format!("stream:{org_id}"),
                    org_id,
                    bypass_check: true,
                    parent_id: folder,
                }));
            }
            return ready(Err(actix_web::error::ErrorUnauthorized(
                "Unauthorized Access",
            )));
        }

        // get ofga object type from the url
        // depends on the url path count
        let object_type = if url_len == 1 {
            // for organization entity itself, get requires the list
            // permissions, and the object is a special format string
            if path_columns[0].eq("organizations") {
                if method.eq("GET") {
                    method = "LIST".to_string();
                };

                "org:##user_id##".to_string()
            } else {
                path_columns[0].to_string()
            }
        } else if url_len == 2 || (url_len > 2 && path_columns[1].eq("settings")) {
            // for settings, the post/delete require PUT permissions, GET needs LIST permissions
            // also the special settings exception is for 3-part urls for logo /text
            // which are of path /org/settings/logo , which need permission of operating
            // on permission in general
            if path_columns[1].eq("settings") {
                if method.eq("POST") || method.eq("DELETE") {
                    method = "PUT".to_string();
                }
            } else if method.eq("GET") {
                method = "LIST".to_string();
            }
            // this will take format of settings:{org_id} or pipelines:{org_id} etc
            let key = if path_columns[1].eq("invites") {
                "users"
            } else if (path_columns[1].eq("rename") || path_columns[1].eq("extend_trial_period"))
                && method.eq("PUT")
            {
                "organizations"
            } else {
                path_columns[1]
            };

            // for organization api changes we need perms on _all_{org}
            let entity = match (key, path_columns[1]) {
                ("organizations", "extend_trial_period") => "_all__meta".to_string(),
                ("organizations", "organizations") => path_columns[0].to_string(),
                ("organizations", _) => format!("_all_{}", path_columns[0]),
                _ => path_columns[0].to_string(),
            };

            format!(
                "{}:{}",
                OFGA_MODELS.get(key).map_or(key, |model| model.key),
                entity
            )
        } else if path_columns[1].eq("groups") || path_columns[1].eq("roles") {
            // for groups or roles, path will be of format /org/roles/id , so we need
            // to check permission on role:org/id for permissions on that specific role
            format!(
                "{}:{org_id}/{}",
                OFGA_MODELS
                    .get(path_columns[1])
                    .map_or(path_columns[1], |model| model.key),
                path_columns[2]
            )
        } else if url_len == 3 {
            // Handle /v2 alert apis
            if path_columns[0].eq(V2_API_PREFIX) && path_columns[2].eq("alerts") {
                if method.eq("GET") {
                    method = "LIST".to_string();
                }
                format!(
                    "{}:{}",
                    OFGA_MODELS.get("alert_folders").unwrap().key,
                    folder
                )
            } else if path_columns[1] == "re_patterns" && path_columns[2] == "test" {
                // specifically for testing re_patterns we need get permissions
                // on re patterns
                method = "LIST".to_string();
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[0]
                )
            }
            // these are cases where the entity is "sub-entity" of some other entity,
            // for example, alerts are on route /org/stream/alerts
            // or templates are on route /org/alerts/templates and so on
            // users/roles is one of the special exception here
            else if path_columns[2].eq("alerts")
                || path_columns[2].eq("templates")
                || path_columns[2].eq("destinations")
                || path.ends_with("users/roles")
            {
                if method.eq("GET") {
                    method = "LIST".to_string();
                }
                if method.eq("PUT") || method.eq("DELETE") || path_columns[1].eq("search_jobs") {
                    // for put/delete actions i.e. updations, we need permissions
                    // on that particular "sub-entity", and this will take form of
                    // alert:templates or alerts:destinations or stream:alerts
                    // search jobs also fall under this 3 length case
                    format!(
                        "{}:{}",
                        OFGA_MODELS
                            .get(path_columns[1])
                            .map_or(path_columns[1], |model| model.key),
                        path_columns[2]
                    )
                } else {
                    // otherwise for listing/creating we need permissions on that "sub-entity"
                    // in general such as org:templates or org:destinations or org:alerts
                    format!(
                        "{}:{}",
                        OFGA_MODELS
                            .get(path_columns[2])
                            .map_or(path_columns[2], |model| model.key),
                        path_columns[0]
                    )
                }
            } else if path_columns[2].starts_with("_values")
                || path_columns[2].starts_with("_around")
            {
                if method.eq("POST") {
                    // For _around search, the rbac check will be "GET"
                    method = "GET".to_string();
                }
                // special case of _values/_around , where we need permission on that stream,
                // as it is part of search, but still 3-part route
                format!(
                    "{}:{}",
                    OFGA_MODELS.get("streams").unwrap().key,
                    path_columns[1]
                )
            } else if path_columns[1].starts_with("rename") {
                // Org rename
                format!(
                    "{}:{}",
                    OFGA_MODELS.get("organizations").unwrap().key,
                    org_id
                )
            } else if (method.eq("PUT") && !path_columns[1].starts_with("ratelimit"))
                || method.eq("DELETE")
                || path_columns[1].eq("reports")
                || path_columns[1].eq("savedviews")
                || path_columns[1].eq("functions")
                || path_columns[1].eq("service_accounts")
                || path_columns[1].eq("cipher_keys")
            {
                // Similar to the alerts/templates etc, but for other entities such as specific
                // pipeline, specific stream, specific alert/destination etc.
                // and these are not "sub-entities" under some other entities, hence
                // a separate else-if clause
                // Similarly, for the put/delete or any operation on these
                // entities, we need access to that particular item
                // so url will be of form /org/reports/name or /org/functions/name etc.
                // nd this will take form name:reports or name:function
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[2]
                )
            } else if method.eq("GET")
                && (path_columns[1].eq("dashboards")
                    || path_columns[1].eq("folders")
                    || path_columns[1].eq("actions"))
            {
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[2] // dashboard id
                )
            } else {
                // for things like dashboards and folders etc,
                // this will take form org:dashboard or org:folders

                // handle ratelimit:org
                if method.eq("GET") && path_columns[1].starts_with("ratelimit") {
                    method = "LIST".to_string();
                }

                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[0]
                )
            }
        } else if url_len == 4 {
            // Handle /v2 alert apis
            if path_columns[0].eq(V2_API_PREFIX) {
                if path_columns[2].eq("alerts") {
                    format!(
                        "{}:{}",
                        OFGA_MODELS
                            .get(path_columns[2])
                            .map_or(path_columns[2], |model| model.key),
                        path_columns[3]
                    )
                } else {
                    if method.eq("GET") {
                        method = "LIST".to_string();
                    }
                    let ofga_type = if path_columns[3].eq("alerts") {
                        "alert_folders"
                    } else {
                        "folders"
                    };
                    format!(
                        "{}:{}",
                        OFGA_MODELS
                            .get(ofga_type)
                            .map_or(ofga_type, |model| model.key),
                        path_columns[1]
                    )
                }
            }
            // this is for specific sub-items like specific alert, destination etc.
            // and sub-items such as schema, stream settings, or enabling/triggering reports
            else if method.eq("PUT") && path_columns[1].eq("reports") {
                // for report enable/trigger, we need permissions on that specific
                // report, so this will be name:reports
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[2]
                )
            } else if method.eq("PUT")
                && path_columns[1] != "streams"
                && path_columns[1] != "pipelines"
                || method.eq("DELETE") && path_columns[3] != "annotations"
            {
                // for put on on-stream, non-pipeline such as specific alert/template/destination
                // or delete on any such (stream/pipeline delete are not 4-part routes)
                // this will take form of name:alert or name:destination or name:template etc
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[2])
                        .map_or(path_columns[2], |model| model.key),
                    path_columns[3]
                )
            } else if method.eq("GET")
                && path_columns[1].eq("folders")
                && path_columns[2].eq("name")
            {
                // To search with folder name, you need GET permission on all folders
                format!(
                    "{}:_all_{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[0]
                )
            } else if method.eq("GET")
                && path_columns[1].eq("actions")
                && path_columns[2].eq("download")
            {
                // To access actions download name, you need GET permission on actions
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[3]
                )
            } else if method.eq("GET")
                && (path_columns[2].eq("templates")
                    || path_columns[2].eq("destinations")
                    || path_columns[2].eq("alerts"))
            {
                // To access templates, you need GET permission on the template
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[2])
                        .map_or(path_columns[2], |model| model.key),
                    path_columns[3]
                )
            } else {
                // for other get/put requests on any entities such as templates,
                // alerts, enable pipeline, update dashboard etc, we need permission
                // on that entity in general, this will take form of
                // alerts:destinations or roles:role_name or stream_name:alerts etc
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[2]
                )
            }
        } else if method.eq("PUT") || method.eq("DELETE") || method.eq("PATCH") {
            // this block is for all other urls
            // specifically checking PUT /org_id/streams/stream_name/delete_fields
            // even though method is put, we actually need to check delete permissions
            if path_columns[url_len - 1].eq("delete_fields") {
                method = "DELETE".to_string();
            }

            if method.eq("PATCH") {
                method = "PUT".to_string();
            }

            // Handle /v2 folders apis
            if path_columns[0].eq(V2_API_PREFIX) && path_columns[2].eq("folders") {
                let ofga_type = if path_columns[3].eq("alerts") {
                    "alert_folders"
                } else {
                    "folders"
                };
                if url_len == 6 {
                    // Should check for all_org permissions
                    format!(
                        "{}:{}",
                        OFGA_MODELS.get(ofga_type).unwrap().key,
                        path_columns[1]
                    )
                } else {
                    format!(
                        "{}:{}",
                        OFGA_MODELS.get(ofga_type).unwrap().key,
                        path_columns[4]
                    )
                }
            }
            //  this is specifically for enabling alerts
            else if path_columns[url_len - 1].eq("enable") {
                // this will take form name:alert
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[2])
                        .map_or(path_columns[2], |model| model.key),
                    path_columns[3]
                )
            } else {
                // This is specifically for triggering the alert on url
                // /org_id/stream_name/alerts/alert_name/trigger
                // and will take form stream_name:alerts
                format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(path_columns[1])
                        .map_or(path_columns[1], |model| model.key),
                    path_columns[2]
                )
            }
        } else {
            // This is the final catch-all for what did not fit in above cases,
            // and for the prometheus urls this will be ignored below.
            format!(
                "{}:{}",
                OFGA_MODELS
                    .get(path_columns[1])
                    .map_or(path_columns[1], |model| model.key),
                path_columns[2]
            )
        };

        // Check if the ws request is using internal grpc token
        if method.eq("GET")
            && path.contains("/ws")
            && let Some(auth_header) = req.headers().get("Authorization")
            && auth_header
                .to_str()
                .unwrap()
                .eq(&get_config().grpc.internal_grpc_token)
        {
            return ready(Ok(AuthExtractor {
                auth: auth_header.to_str().unwrap().to_string(),
                method,
                o2_type: format!("stream:{org_id}"),
                org_id,
                bypass_check: true,
                parent_id: folder,
            }));
        }

        let auth_str = extract_auth_str(req);

        // if let Some(auth_header) = req.headers().get("Authorization") {
        if !auth_str.is_empty() {
            if (method.eq("POST") && url_len > 1 && path_columns[1].starts_with("_search"))
                || (method.eq("POST") && url_len > 1 && path.ends_with("actions/upload"))
                || path.contains("/prometheus/api/v1/query")
                || path.contains("/resources")
                || path.contains("/format_query")
                || path.contains("/prometheus/api/v1/series")
                || path.contains("/traces/latest")
                || path.contains("clusters")
                || path.contains("query_manager")
                || path.contains("/short")
                || path.contains("/ws")
                || path.contains("/_values_stream")
            {
                return ready(Ok(AuthExtractor {
                    auth: auth_str.to_owned(),
                    method: "".to_string(),
                    o2_type: "".to_string(),
                    org_id: "".to_string(),
                    bypass_check: true, // bypass check permissions
                    parent_id: folder,
                }));
            }
            if object_type.starts_with("stream") {
                let object_type = match stream_type {
                    Some(stream_type) => {
                        if stream_type.eq(&StreamType::EnrichmentTables) {
                            // since enrichment tables have separate permissions
                            let stream_type_str = format!("{stream_type}");

                            object_type.replace(
                                "stream:",
                                format!(
                                    "{}:",
                                    OFGA_MODELS
                                        .get(stream_type_str.as_str())
                                        .map_or(stream_type_str.as_str(), |model| model.key)
                                )
                                .as_str(),
                            )
                        } else {
                            object_type.replace("stream:", format!("{stream_type}:").as_str())
                        }
                    }
                    None => object_type,
                };
                return ready(Ok(AuthExtractor {
                    auth: auth_str.to_owned(),
                    method,
                    o2_type: object_type,
                    org_id,
                    bypass_check: false,
                    parent_id: folder,
                }));
            }
            if object_type.contains("dashboard") && url_len > 1 {
                let object_type = if method.eq("POST") || method.eq("LIST") {
                    format!(
                        "{}:{}",
                        OFGA_MODELS
                            .get(path_columns[1])
                            .map_or("dfolder", |model| model.parent),
                        folder.as_str(),
                    )
                } else {
                    object_type
                };
                // Currently, we have a patch api for dashboard move,
                // which can not be handled by the middleware layer,
                // so we need to bypass the check here
                if method.eq("PATCH") {
                    return ready(Ok(AuthExtractor {
                        auth: auth_str.to_owned(),
                        method: "".to_string(),
                        o2_type: "".to_string(),
                        org_id: "".to_string(),
                        bypass_check: true, // bypass check permissions
                        parent_id: folder,
                    }));
                }

                return ready(Ok(AuthExtractor {
                    auth: auth_str.to_owned(),
                    method,
                    o2_type: object_type,
                    org_id,
                    bypass_check: false,
                    parent_id: folder,
                }));
            }

            if method.eq("PATCH") && object_type.eq("alert:move") {
                return ready(Ok(AuthExtractor {
                    auth: auth_str.to_owned(),
                    method: "".to_string(),
                    o2_type: "".to_string(),
                    org_id: "".to_string(),
                    bypass_check: true, // bypass check permissions
                    parent_id: folder,
                }));
            }

            return ready(Ok(AuthExtractor {
                auth: auth_str.to_owned(),
                method,
                o2_type: object_type,
                org_id,
                bypass_check: false,
                parent_id: folder,
            }));
        }
        log::info!(
            "AuthExtractor::from_request took {} ms",
            start.elapsed().as_millis()
        );
        ready(Err(actix_web::error::ErrorUnauthorized(
            "Unauthorized Access",
        )))
    }

    #[cfg(not(feature = "enterprise"))]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let auth_str = if let Some(cookie) = req.cookie("auth_tokens") {
            let val = config::utils::base64::decode_raw(cookie.value()).unwrap_or_default();
            let auth_tokens: AuthTokens =
                json::from_str(std::str::from_utf8(&val).unwrap_or_default()).unwrap_or_default();
            let access_token = auth_tokens.access_token;
            if access_token.starts_with("Basic") || access_token.starts_with("Bearer") {
                access_token
            } else {
                format!("Bearer {access_token}")
            }
        } else if let Some(auth_header) = req.headers().get("Authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                auth_str.to_owned()
            } else {
                "".to_string()
            }
        } else {
            "".to_string()
        };

        // if let Some(auth_header) = req.headers().get("Authorization") {
        if !auth_str.is_empty() {
            return ready(Ok(AuthExtractor {
                auth: auth_str.to_owned(),
                method: "".to_string(),
                o2_type: "".to_string(),
                org_id: "".to_string(),
                bypass_check: true, // bypass check permissions
                parent_id: "".to_string(),
            }));
        }

        ready(Err(actix_web::error::ErrorUnauthorized(
            "Unauthorized Access",
        )))
    }
}

#[cfg(feature = "enterprise")]
pub fn extract_auth_str(req: &HttpRequest) -> String {
    let auth_ext_cookie = |req: &HttpRequest| -> String {
        req.cookie("auth_ext")
            .map(|cookie| {
                let val = config::utils::base64::decode_raw(cookie.value()).unwrap_or_default();
                std::str::from_utf8(&val).unwrap_or_default().to_string()
            })
            .unwrap_or_default()
    };

    if let Some(cookie) = req.cookie("auth_tokens") {
        let val = config::utils::base64::decode_raw(cookie.value()).unwrap_or_default();
        let auth_tokens: AuthTokens =
            json::from_str(std::str::from_utf8(&val).unwrap_or_default()).unwrap_or_default();
        let access_token = auth_tokens.access_token;
        if access_token.is_empty() {
            // If cookie was set but access token is still empty
            // we check auth_ext cookie to get the token.
            auth_ext_cookie(req)
        } else if access_token.starts_with("Basic") || access_token.starts_with("Bearer") {
            access_token
        } else if access_token.starts_with("session") {
            let session_key = access_token.strip_prefix("session ").unwrap().to_string();
            match USER_SESSIONS.get(&session_key) {
                Some(token) => {
                    format!("Bearer {}", *token)
                }
                None => access_token,
            }
        } else {
            format!("Bearer {access_token}")
        }
    } else if let Some(cookie) = req.cookie("auth_ext") {
        let val = config::utils::base64::decode_raw(cookie.value()).unwrap_or_default();
        std::str::from_utf8(&val).unwrap_or_default().to_string()
    } else if let Some(auth_header) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            auth_str.to_owned()
        } else {
            "".to_string()
        }
    } else {
        "".to_string()
    }
}

/// Constructs the login URL with the provided parameters.
///
/// # Arguments
///
/// * `base_url` - The base URL of the authentication service.
/// * `time` - The request time.
/// * `exp_in` - The expiration time.
/// * `auth` - The authentication token.
///
/// # Returns
///
/// The constructed login URL.
pub fn generate_presigned_url(
    username: &str,
    password: &str,
    salt: &str,
    base_url: &str,
    exp_in: i64,
    time: i64,
) -> String {
    // let time = chrono::Utc::now().timestamp();
    let stage1 = get_hash(password, salt);
    let stage2 = get_hash(&format!("{}{}", &stage1, time), salt);
    let stage3 = get_hash(&format!("{}{}", &stage2, exp_in), salt);

    let user_pass = format!("{username}:{stage3}");
    let auth = base64::engine::general_purpose::STANDARD.encode(user_pass);

    format!("{base_url}/auth/login?request_time={time}&exp_in={exp_in}&auth={auth}")
}

#[cfg(not(feature = "enterprise"))]
pub async fn check_permissions(
    _object_id: Option<String>,
    _org_id: &str,
    _user_id: &str,
    _object_type: &str,
    _method: &str,
) -> bool {
    false
}

/// Returns false if Auth fails
#[cfg(feature = "enterprise")]
pub async fn check_permissions(
    object_id: Option<String>,
    org_id: &str,
    user_id: &str,
    object_type: &str,
    method: &str,
    parent_id: &str,
) -> bool {
    if !is_root_user(user_id) {
        let user: config::meta::user::User = match get_user(Some(org_id), user_id).await {
            Some(user) => user.clone(),
            None => return false,
        }
        .clone();

        let object_id = match object_id {
            Some(id) => id,
            None => org_id.to_string(),
        };

        return crate::handler::http::auth::validator::check_permissions(
            user_id,
            AuthExtractor {
                auth: "".to_string(),
                method: method.to_string(),
                o2_type: format!(
                    "{}:{}",
                    OFGA_MODELS
                        .get(object_type)
                        .map_or(object_type, |model| model.key),
                    object_id
                ),
                org_id: org_id.to_string(),
                bypass_check: false,
                parent_id: parent_id.to_string(),
            },
            user.role,
            user.is_external,
        )
        .await;
    }
    true
}

#[cfg(feature = "enterprise")]
pub async fn extract_auth_expiry_and_user_id(
    req: &HttpRequest,
) -> (Option<chrono::DateTime<chrono::Utc>>, Option<String>) {
    use crate::handler::http::auth::validator::get_user_email_from_auth_str;

    let decode = async |token: &str| match decode_expiry(token).await {
        Ok(token_data) => token_data
            .claims
            .get("exp")
            .and_then(|exp| exp.as_i64())
            .and_then(|exp_ts| chrono::DateTime::from_timestamp(exp_ts, 0)),
        Err(e) => {
            log::error!("Error verifying token: {e}");
            None
        }
    };

    let auth_str = extract_auth_str(req);
    if auth_str.is_empty() {
        return (None, None);
    } else if auth_str.starts_with("Basic") {
        let user_id = get_user_email_from_auth_str(&auth_str).await;
        return (None, user_id);
    } else if auth_str.starts_with("Bearer") {
        let user_id = get_user_email_from_auth_str(&auth_str).await;
        let stripped_bearer_token = auth_str.strip_prefix("Bearer ").unwrap();
        let exp = decode(stripped_bearer_token).await;
        return (exp, user_id);
    } else if auth_str.starts_with("session ") {
        let session_key = auth_str.strip_prefix("session ").unwrap();
        let stripped_bearer_token = match crate::service::db::session::get(session_key).await {
            Ok(bearer_token) => bearer_token,
            Err(e) => {
                log::error!("Error getting session: {e}");
                return (None, None);
            }
        };
        let exp = decode(&stripped_bearer_token).await;
        let bearer_full_token = format!("Bearer {stripped_bearer_token}");
        let user_id = get_user_email_from_auth_str(&bearer_full_token).await;
        return (exp, user_id);
    }
    (None, None)
}

#[cfg(feature = "enterprise")]
async fn decode_expiry(token: &str) -> Result<TokenData<HashMap<String, Value>>, anyhow::Error> {
    use infra::errors::JwtError;
    use jsonwebtoken::{
        Algorithm, DecodingKey, Validation, decode, decode_header,
        jwk::{self, AlgorithmParameters},
    };

    let header = decode_header(token)?;
    let kid = match header.kid {
        Some(k) => k,
        None => return Err(JwtError::MissingAttribute("`kid` header".to_owned()).into()),
    };
    let dex_jwks = get_dex_jwks().await;
    let jwks: jwk::JwkSet = serde_json::from_str(&dex_jwks).unwrap();

    if let Some(j) = jwks.find(&kid) {
        match &j.algorithm {
            AlgorithmParameters::RSA(rsa) => {
                let decoding_key = DecodingKey::from_rsa_components(&rsa.n, &rsa.e).unwrap();

                let mut validation = Validation::new(
                    Algorithm::from_str(j.common.key_algorithm.unwrap().to_string().as_str())
                        .unwrap(),
                );
                validation.validate_exp = true;
                let aud = &o2_dex::config::get_config().client_id;
                validation.set_audience(&[aud]);
                Ok(decode::<HashMap<String, serde_json::Value>>(
                    token,
                    &decoding_key,
                    &validation,
                )?)
            }
            _ => Err(JwtError::ValidationFailed().into()),
        }
    } else {
        Err(JwtError::KeyNotExists().into())
    }
}

#[cfg(test)]
mod tests {
    use infra::{db as infra_db, table as infra_table};

    use super::*;
    use crate::{
        common::meta::user::UserRequest,
        service::{self, organization, users},
    };

    #[test]
    fn test_valid_emails() {
        assert!(is_valid_email("user@example.com"));
        assert!(is_valid_email("john.doe+123@mail.co.in"));
        assert!(is_valid_email("a_b-c.d+e@domain.org"));
        assert!(!is_valid_email("no-at-symbol.com"));
        assert!(!is_valid_email("@missing-user.com"));
        assert!(!is_valid_email("user@.com"));
        assert!(!is_valid_email("user@com"));
        assert!(!is_valid_email("user@domain..com"));
    }

    #[test]
    fn test_is_ofga_unsupported() {
        assert!(is_ofga_unsupported("abc:123"));
        assert!(is_ofga_unsupported("name with space"));
        assert!(is_ofga_unsupported("foo&bar"));
        assert!(!is_ofga_unsupported("valid_name"));
        assert!(!is_ofga_unsupported("name_with_underscores"));
    }

    #[test]
    fn test_into_ofga_supported_format() {
        assert_eq!(into_ofga_supported_format("foo:bar"), "foo_bar");
        assert_eq!(into_ofga_supported_format("foo bar"), "foo_bar");
        assert_eq!(into_ofga_supported_format("foo#bar"), "foo_bar");
        assert_eq!(into_ofga_supported_format("foo : bar"), "foo_bar");
        assert_eq!(into_ofga_supported_format(" a  & b "), "_a_b_");
        assert_eq!(into_ofga_supported_format("a   b"), "a_b");
        assert_eq!(into_ofga_supported_format("a:b#c?d e"), "a_b_c_d_e");
        assert_eq!(into_ofga_supported_format("foo & bar % baz"), "foo_bar_baz");
    }

    #[test]
    fn test_generate_presigned_url() {
        let password = "password";
        let salt = "saltsalt";
        let username = "user";
        let base_url = "https://example.com";
        let exp_in = 3600;
        let time = 1634567890;

        let expected_url = format!(
            "{}/auth/login?request_time={}&exp_in={}&auth={}",
            base_url,
            time,
            exp_in,
            "dXNlcjokYXJnb24yZCR2PTE2JG09MjA0OCx0PTQscD0yJGMyRnNkSE5oYkhRJGNwTElHZzdEaFl1Vi9nSWxMaCtRZksrS29Vd2ZFaGVpdHkwc3Z0c243Y1E="
        );

        let generated_url =
            generate_presigned_url(username, password, salt, base_url, exp_in, time);

        assert_eq!(generated_url, expected_url);
    }

    #[tokio::test]
    async fn test_is_root_user() {
        assert!(!is_root_user("dummy"));
    }

    #[tokio::test]
    #[ignore]
    async fn test_is_root_user2() {
        infra_db::create_table().await.unwrap();
        infra_table::create_user_tables().await.unwrap();
        organization::check_and_create_org_without_ofga(DEFAULT_ORG)
            .await
            .unwrap();
        let _ = users::create_root_user_if_not_exists(
            DEFAULT_ORG,
            UserRequest {
                email: "root@example.com".to_string(),
                password: "Complexpass#123".to_string(),
                role: UserOrgRole {
                    base_role: config::meta::user::UserRole::Root,
                    custom_role: None,
                },
                first_name: "root".to_owned(),
                last_name: "".to_owned(),
                is_external: false,
                token: None,
            },
        )
        .await;
        service::db::user::cache().await.unwrap();
        service::db::organization::cache().await.unwrap();
        service::db::org_users::cache().await.unwrap();
        assert!(is_root_user("root@example.com"));
        assert!(!is_root_user("root2@example.com"));
    }

    #[tokio::test]
    async fn test_get_hash() {
        let hash =
            "$argon2d$v=16$m=2048,t=4,p=2$VGVzdFNhbHQ$CZzrFPtqjY4mIPYwoDztCJ3OGD5M0P37GH4QddwrbZk";
        assert_eq!(get_hash("Pass#123", "TestSalt"), hash);
    }

    #[tokio::test]
    async fn test_get_hash_for_pass() {
        let pass1 = get_hash("Pass#123", "openobserve");
        let time = chrono::Utc::now().timestamp();
        let pass2 = get_hash(&format!("{}{}", &pass1, time), "openobserve");
        let exp_in = 600;
        let pass3 = get_hash(&format!("{}{}", &pass2, exp_in), "openobserve");
        println!("time: {time}");
        println!("pass3: {pass3}");

        let user_pass = format!("{}:{}", "b@b.com", pass3);
        let auth = base64::engine::general_purpose::STANDARD.encode(user_pass);
        println!(
            "http://localhost:5080/auth/login?request_time={time}&exp_in={exp_in}&auth={auth}"
        );
    }
}
