<!-- Copyright 2023 Zinc Labs Inc.

 Licensed under the Apache License, Version 2.0 (the "License");
 you may not use this file except in compliance with the License.
 You may obtain a copy of the License at

     http:www.apache.org/licenses/LICENSE-2.0

 Unless required by applicable law or agreed to in writing, software
 distributed under the License is distributed on an "AS IS" BASIS,
 WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 See the License for the specific language governing permissions and
 limitations under the License. 
-->

<template>
  <div class="sessions_page">
    <div class="text-right q-py-sm flex align-center justify-between">
      <syntax-guide class="q-mr-sm" />
      <div class="flex align-center justify-end metrics-date-time q-mr-md">
        <date-time
          auto-apply
          :default-type="sessionState.data.datetime.valueType"
          :default-absolute-time="{
            startTime: sessionState.data.datetime.startTime,
            endTime: sessionState.data.datetime.endTime,
          }"
          :default-relative-time="sessionState.data.datetime.relativeTimePeriod"
          data-test="logs-search-bar-date-time-dropdown"
          class="q-mr-md"
          @on:date-change="updateDateChange"
        />
        <q-btn
          data-test="metrics-explorer-run-query-button"
          data-cy="metrics-explorer-run-query-button"
          dense
          flat
          title="Run query"
          class="q-pa-none search-button"
          @click="runQuery"
        >
          Run query
        </q-btn>
      </div>
    </div>
    <query-editor
      editorId="session-replay-query-editor"
      class="monaco-editor"
      v-model:query="sessionState.data.editorValue"
      :debounce-time="300"
    />
    <q-splitter
      class="logs-horizontal-splitter full-height"
      v-model="splitterModel"
      unit="px"
      vertical
    >
      <template #before>
        <FieldList
          :fields="streamFields"
          :time-stamp="{
            startTime: dateTime.startTime,
            endTime: dateTime.endTime,
          }"
          :stream-name="rumSessionStreamName"
          @event-emitted="handleSidebarEvent"
        />
      </template>
      <template #separator>
        <q-avatar
          color="primary"
          text-color="white"
          size="20px"
          icon="drag_indicator"
          style="top: 10px"
        />
      </template>
      <template #after>
        <template v-if="isLoading.length">
          <div
            class="q-pb-lg flex items-center justify-center text-center"
            style="height: calc(100vh - 200px)"
          >
            <div>
              <q-spinner-hourglass
                color="primary"
                size="40px"
                style="margin: 0 auto; display: block"
              />
              <div class="text-center full-width">
                Hold on tight, we're fetching your sessions.
              </div>
            </div>
          </div>
        </template>
        <template v-else>
          <AppTable
            :columns="columns"
            :rows="rows"
            class="app-table-container"
            @event-emitted="handleTableEvents"
          >
            <template v-slot:session_location_column="slotProps">
              <SessionLocationColumn :column="slotProps.column.row" />
            </template>
          </AppTable>
        </template>
      </template>
    </q-splitter>
  </div>
</template>

<script setup lang="ts">
import { onActivated, ref } from "vue";
import { useI18n } from "vue-i18n";
import AppTable from "@/components/AppTable.vue";
import {
  formatDuration,
  b64DecodeUnicode,
  b64EncodeUnicode,
} from "@/utils/zincutils";
import FieldList from "@/components/common/sidebar/FieldList.vue";
import { useRouter } from "vue-router";
import streamService from "@/services/stream";
import { useStore } from "vuex";
import useQuery from "@/composables/useQuery";
import searchService from "@/services/search";
import { date } from "quasar";
import useSession from "@/composables/useSessionReplay";
import QueryEditor from "@/components/QueryEditor.vue";
import DateTime from "@/components/DateTime.vue";
import SyntaxGuide from "@/plugins/traces/SyntaxGuide.vue";
import SessionLocationColumn from "@/components/rum/sessionReplay/SessionLocationColumn.vue";

interface Session {
  timestamp: string;
  type: string;
  time_spent: number;
  error_count: string;
  initial_view_name: string;
  id: string;
}

const streamFields = ref([]);
const { getTimeInterval, buildQueryPayload, parseQuery } = useQuery();

const { sessionState } = useSession();
const store = useStore();
const isLoading = ref<boolean[]>([]);
const { t } = useI18n();
const dateTime = ref({
  startTime: 0,
  endTime: 0,
});
const rumSessionStreamName = "_sessionreplay";

const columns = ref([
  {
    name: "action_play",
    field: "",
    label: "",
    type: "action",
    icon: "play_circle_filled",
    style: { width: "56px" },
  },
  {
    name: "timestamp",
    field: (row: any) => getFormattedDate(row["timestamp"] / 1000),
    label: "Timestamp",
    align: "left",
    sortable: true,
  },
  {
    name: "user_email",
    field: (row: any) => row["user_email"] || "Unknown",
    label: "User Email",
    align: "left",
    sortable: true,
  },
  {
    name: "time_spent",
    field: (row: any) => formatDuration(row["time_spent"]),
    label: "Time Spent",
    align: "left",
    sortable: true,
    sort: (a: any, b: any, rowA: Session, rowB: Session) => {
      return (
        parseInt(rowA.time_spent.toString()) -
        parseInt(rowB.time_spent.toString())
      );
    },
  },
  {
    name: "error_count",
    field: (row: any) => row["error_count"],
    prop: (row: any) => row["error_count"],
    label: "Error Count",
    align: "left",
    sortable: true,
  },
  {
    name: "location",
    field: (row: any) => formatDuration(row["time_spent"]),
    label: "Location",
    align: "left",
    slot: true,
    slotName: "session_location_column",
    sortable: true,
    sort: (a: any, b: any, rowA: Session, rowB: Session) => {
      return (
        parseInt(rowA.time_spent.toString()) -
        parseInt(rowB.time_spent.toString())
      );
    },
  },
]);

onActivated(async () => {
  if (router.currentRoute.value.name === "Sessions") {
    await getStreamFields();
    getSessions();
    restoreUrlQueryParams();
  }
});

const getStreamFields = () => {
  isLoading.value.push(true);
  return new Promise((resolve) => {
    streamService
      .schema(
        store.state.selectedOrganization.identifier,
        rumSessionStreamName,
        "logs"
      )
      .then((res) => {
        streamFields.value = res.data.schema.map((field: any) => ({
          ...field,
          showValues: true,
        }));
      })
      .finally(() => {
        resolve(true);
        isLoading.value.pop();
      });
  });
};

const getSessions = () => {
  const interval = getTimeInterval(
    dateTime.value.startTime,
    dateTime.value.endTime
  );
  const parsedQuery = parseQuery(sessionState.data.editorValue, false);

  const queryPayload: any = {
    from: Object.keys(sessionState.data.sessions).length,
    size: sessionState.data.resultGrid.size,
    timestamp_column: store.state.zoConfig.timestamp_column,
    timestamps: {
      startTime: dateTime.value.startTime,
      endTime: dateTime.value.endTime,
    },
    timeInterval: interval.interval,
    sqlMode: false,
    currentPage: sessionState.data.resultGrid.currentPage,
    selectedStream: rumSessionStreamName,
    parsedQuery,
    streamName: rumSessionStreamName,
  };

  const req = buildQueryPayload(queryPayload);

  req.query.sql = `select min(${
    store.state.zoConfig.timestamp_column
  }) as zo_sql_timestamp, min(start) as start_time, max(end) as end_time, min(user_agent_user_agent_family) as browser, min(user_agent_os_family) as os, min(ip) as ip, min(source) as source, session_id from "_sessionreplay" ${
    sessionState.data.editorValue.length
      ? " where " + sessionState.data.editorValue
      : ""
  } group by session_id order by zo_sql_timestamp DESC`;
  req.query.sql_mode = "full";
  delete req.aggs;
  isLoading.value.push(true);

  updateUrlQueryParams();

  searchService
    .search({
      org_identifier: store.state.selectedOrganization.identifier,
      query: req,
      page_type: "logs",
    })
    .then((res) => {
      res.data.hits.forEach((hit: any) => {
        sessionState.data.sessions[hit.session_id] = hit;
        sessionState.data.sessions[hit.session_id]["type"] = hit.source;
        sessionState.data.sessions[hit.session_id]["time_spent"] =
          hit.end_time - hit.start_time;
        sessionState.data.sessions[hit.session_id]["timestamp"] =
          hit.zo_sql_timestamp;
      });

      getSessionLogs(req);
    })
    .finally(() => isLoading.value.pop());
};

const getSessionLogs = (req: any) => {
  req.query.sql = `select min(${store.state.zoConfig.timestamp_column}) as zo_sql_timestamp, min(type) as type, SUM(CASE WHEN type='error' THEN 1 ELSE 0 END) AS error_count, SUM(CASE WHEN type!='null' THEN 1 ELSE 0 END) AS events, min(usr_email) as user_email, min(usr_id) as user_id, min(geo_info_city) as city, min(geo_info_country) as country, session_id from "_rumdata" group by session_id order by zo_sql_timestamp DESC`;

  isLoading.value.push(true);
  searchService
    .search({
      org_identifier: store.state.selectedOrganization.identifier,
      query: req,
      page_type: "logs",
    })
    .then((res) => {
      const hits = res.data.hits;
      hits.forEach((hit: any) => {
        if (sessionState.data.sessions[hit.session_id]) {
          sessionState.data.sessions[hit.session_id].error_count =
            hit.error_count;
          sessionState.data.sessions[hit.session_id].user_email =
            hit.user_email;
          sessionState.data.sessions[hit.session_id].country = hit.country;
          sessionState.data.sessions[hit.session_id].city = hit.city;
        }
      });
      rows.value = Object.values(sessionState.data.sessions);
    })
    .finally(() => isLoading.value.pop());
};

const updateDateChange = (date: any) => {
  if (JSON.stringify(date) === JSON.stringify(dateTime.value)) return;
  dateTime.value = date;
  sessionState.data.datetime = date;
  getSessions();
};

const splitterModel = ref(250);

const rows = ref<Session[]>([]);
const router = useRouter();

const handleTableEvents = (event: string, payload: any) => {
  const eventMapping: { [key: string]: (payload: any) => void } = {
    "cell-click": handleCellClick,
    scroll: handleScroll,
  };
  eventMapping[event](payload);
};

const handleScroll = (scrollData: any) => {
  if (!isLoading.value.length) {
    const totalFetchedSessions = Object.keys(sessionState.data.sessions).length;
    if (totalFetchedSessions / scrollData.to < 1.3) {
      sessionState.data.resultGrid.currentPage++;
      // getSessions();
    }
  }
};

const handleCellClick = (payload: any) => {
  if (payload.columnName !== "action_play") return;
  sessionState.data.selectedSession =
    sessionState.data.sessions[payload.row.session_id];
  router.push({
    name: "SessionViewer",
    params: { id: payload.row.session_id },
    query: {
      start_time: payload.row.start_time * 1000,
      end_time: payload.row.end_time * 1000,
    },
  });
};

const handleSidebarEvent = (event: string, value: any) => {
  if (event === "add-field") {
    if (sessionState.data.editorValue.length) {
      sessionState.data.editorValue += " and " + value;
    } else {
      sessionState.data.editorValue += value;
    }
  }
};

const getFormattedDate = (timestamp: number) =>
  date.formatDate(Math.floor(timestamp), "MMM DD, YYYY HH:mm:ss Z");

const runQuery = () => {
  sessionState.data.resultGrid.currentPage = 0;
  sessionState.data.sessions = {};
  getSessions();
};

function restoreUrlQueryParams() {
  const queryParams = router.currentRoute.value.query;

  const date = {
    startTime: Number(queryParams.from) as number,
    endTime: Number(queryParams.to) as number,
    relativeTimePeriod: (queryParams.period as string) || "",
    valueType: queryParams.period ? "relative" : "absolute",
  };

  if (date && ((date.startTime && date.endTime) || date.relativeTimePeriod)) {
    sessionState.data.datetime = date;
  }

  if (queryParams.query) {
    sessionState.data.editorValue =
      b64DecodeUnicode(queryParams.query as string) || "";
  }
}

function updateUrlQueryParams() {
  const date = sessionState.data.datetime;
  const query: any = {};

  if (date.valueType == "relative") {
    query["period"] = date.relativeTimePeriod;
  } else {
    query["from"] = date.startTime;
    query["to"] = date.endTime;
  }

  query["query"] = b64EncodeUnicode(sessionState.data.editorValue);

  query["org_identifier"] = store.state.selectedOrganization.identifier;
  router.push({ query });
}
</script>
<style scoped lang="scss">
.sessions_page {
  .monaco-editor {
    height: 80px !important;
  }

  .app-table-container {
    height: calc(100vh - 224px) !important;
  }
}
</style>
<style lang="scss">
.sessions_page {
  .index-menu .field_list .field_overlay .field_label,
  .q-field__native,
  .q-field__input,
  .q-table tbody td {
    font-size: 12px !important;
  }

  .q-splitter__after {
    overflow: hidden;
  }

  .q-item__label span {
    /* text-transform: capitalize; */
  }

  .index-table :hover::-webkit-scrollbar,
  #tracesSearchGridComponent:hover::-webkit-scrollbar {
    height: 13px;
    width: 13px;
  }

  .index-table ::-webkit-scrollbar-track,
  #tracesSearchGridComponent::-webkit-scrollbar-track {
    -webkit-box-shadow: inset 0 0 6px rgba(0, 0, 0, 0.3);
    border-radius: 10px;
  }

  .index-table ::-webkit-scrollbar-thumb,
  #tracesSearchGridComponent::-webkit-scrollbar-thumb {
    border-radius: 10px;
    -webkit-box-shadow: inset 0 0 6px rgba(0, 0, 0, 0.5);
  }

  .q-table__top {
    padding: 0px !important;
  }

  .q-table__control {
    width: 100%;
  }

  .q-field__control-container {
    padding-top: 0px !important;
  }

  .search-button {
    width: 96px;
    line-height: 29px;
    font-weight: bold;
    text-transform: initial;
    font-size: 11px;
    color: white;

    .q-btn__content {
      background: $secondary;
      border-radius: 3px 3px 3px 3px;

      .q-icon {
        font-size: 15px;
        color: #ffffff;
      }
    }
  }
}
</style>