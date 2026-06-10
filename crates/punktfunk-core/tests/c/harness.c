/*
 * punktfunk-core C ABI harness — M1 acceptance.
 *
 * Proves the core links from C and round-trips encoded access units through the full
 * packetize -> FEC -> in-process loopback (with deterministic packet loss) -> FEC
 * recover -> reassemble path, recovering every byte exactly.
 *
 * Build/run: see tests/c/run.sh (also driven by `cargo test --test c_abi`).
 */
#include "punktfunk_core.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static PunktfunkConfig make_config(uint32_t role, uint32_t drop_period) {
    PunktfunkConfig c;
    memset(&c, 0, sizeof(c));
    c.struct_size = (uint32_t)sizeof(PunktfunkConfig);
    c.role = role;                 /* 0 = host, 1 = client */
    c.phase = 1;                   /* P1, GameStream-compatible */
    c.fec_scheme = 0;              /* GF(2^8) */
    c.fec_percent = 25;
    c.max_data_per_block = 64;
    c.shard_payload = 1024;
    c.max_frame_bytes = 8 * 1024 * 1024;
    c.encrypt = 0;
    c.loopback_drop_period = drop_period;
    return c;
}

int main(void) {
    printf("punktfunk-core C ABI harness (abi_version=%u)\n", punktfunk_abi_version());

    const uint32_t DROP_PERIOD = 8;   /* drop 1 of every 8 packets */
    PunktfunkConfig host_cfg = make_config(0, DROP_PERIOD);
    PunktfunkConfig client_cfg = make_config(1, DROP_PERIOD);

    PunktfunkSession *host = NULL;
    PunktfunkSession *client = NULL;
    PunktfunkStatus rc = punktfunk_test_loopback_pair(&host_cfg, &client_cfg, &host, &client);
    if (rc != PUNKTFUNK_STATUS_OK || !host || !client) {
        fprintf(stderr, "FAIL: loopback_pair rc=%d\n", (int)rc);
        return 1;
    }

    const size_t FRAME_LEN = 200000;  /* ~196 shards across 4 FEC blocks */
    const int FRAMES = 4;
    uint8_t *buf = (uint8_t *)malloc(FRAME_LEN);
    if (!buf) { fprintf(stderr, "FAIL: oom\n"); return 1; }

    int failures = 0;
    for (int f = 0; f < FRAMES; f++) {
        for (size_t i = 0; i < FRAME_LEN; i++) {
            buf[i] = (uint8_t)((i * 131u) + (unsigned)f * 17u);
        }

        rc = punktfunk_host_submit_frame(host, buf, FRAME_LEN, (uint64_t)f * 1000000u, 0);
        if (rc != PUNKTFUNK_STATUS_OK) {
            fprintf(stderr, "FAIL: submit frame %d rc=%d\n", f, (int)rc);
            failures++;
            continue;
        }

        PunktfunkFrame out;
        memset(&out, 0, sizeof(out));
        rc = punktfunk_client_poll_frame(client, &out);
        if (rc != PUNKTFUNK_STATUS_OK) {
            fprintf(stderr, "FAIL: poll frame %d rc=%d (expected recovery)\n", f, (int)rc);
            failures++;
            continue;
        }
        if (out.len != FRAME_LEN || memcmp(out.data, buf, FRAME_LEN) != 0) {
            fprintf(stderr, "FAIL: frame %d mismatch (len=%zu want=%zu)\n",
                    f, (size_t)out.len, FRAME_LEN);
            failures++;
            continue;
        }
        if (out.frame_index != (uint32_t)f) {
            fprintf(stderr, "FAIL: frame %d wrong index %u\n", f, out.frame_index);
            failures++;
        }
    }

    PunktfunkStats st;
    memset(&st, 0, sizeof(st));
    punktfunk_get_stats(client, &st);
    printf("client stats: completed=%llu recovered_shards=%llu dropped_pkts=%llu rx_pkts=%llu\n",
           (unsigned long long)st.frames_completed,
           (unsigned long long)st.fec_recovered_shards,
           (unsigned long long)st.packets_dropped,
           (unsigned long long)st.packets_received);

    if (st.fec_recovered_shards == 0) {
        fprintf(stderr, "FAIL: expected FEC to recover lost shards, but recovered 0\n");
        failures++;
    }

    free(buf);
    punktfunk_session_free(host);
    punktfunk_session_free(client);

    if (failures == 0) {
        printf("PASS: %d frames round-tripped byte-exact through lossy loopback\n", FRAMES);
        return 0;
    }
    fprintf(stderr, "FAILED with %d errors\n", failures);
    return 1;
}
