<!-- Copyright 2023 OpenObserve Inc.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
-->

<template>
  <div class="q-ma-md">
    <CopyContent class="q-mt-sm" :content="content" />
  </div>
</template>

<script lang="ts">
import { defineComponent, ref, type Ref } from "vue";
import config from "../../../aws-exports";
import { useStore } from "vuex";
import { getEndPoint, getImageURL, getIngestionURL } from "../../../utils/zincutils";
import CopyContent from "@/components/CopyContent.vue";

export default defineComponent({
  name: "traces-otlp",
  props: {
    currOrgIdentifier: {
      type: String,
    },
    currUserEmail: {
      type: String,
    },
  },
  components: { CopyContent },
  setup(props) {
    const store = useStore();
    const endpoint: any = ref({
      url: "",
      host: "",
      port: "",
      protocol: "",
      tls: "",
    });
    const ingestionURL = getIngestionURL();
    endpoint.value = getEndPoint(ingestionURL);

    const content = `[[outputs.http]]
  ## URL is the address to send metrics to
  url = "${endpoint.value.url}/api/${store.state.selectedOrganization.identifier}/prometheus/api/v1/write"
  ## Data format to output.
  data_format = "prometheusremotewrite"

  [outputs.http.headers]
     Content-Type = "application/x-protobuf"
     Content-Encoding = "snappy"
     X-Prometheus-Remote-Write-Version = "0.1.0"
     Authorization = "Basic [BASIC_PASSCODE]"`;

    return {
      store,
      config,
      endpoint,
      content,
      getImageURL,
    };
  },
});
</script>
