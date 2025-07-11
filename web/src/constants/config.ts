// Copyright 2023 OpenObserve Inc.
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

export const config = {
  userPoolsId: import.meta.env.VITE_USER_POOLS_ID,
  webClientId: import.meta.env.VITE_WEB_CLIENT_ID,
  oAuthDomain: import.meta.env.VITE_OAUTH_DOMAIN,
  redirectSignIn: import.meta.env.VITE_REDIRECT_SIGNIN,
  redirectSignOut: import.meta.env.VITE_REDIRECT_SIGNOUT,
  responseType: "CODE",
  scope: "",
};

export const siteURL = {
  contactSales: "https://openobserve.ai/contactus/",
  contactSupport: "https://openobserve.ai/contactus/"
}
