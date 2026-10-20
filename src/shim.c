/* g_api is a thread-local exported by the server; stable Rust cannot read an extern TLS symbol. */
#define _GNU_SOURCE
#include <ls.h>
#include <dlfcn.h>

const lsi_api_t *hatch_api(void) { return g_api; }

/* CyberPanel's core adds three members to lsi_api_t and exports this marker. */
int hatch_api_marker_v1(void)
{
    return dlsym(RTLD_DEFAULT, "cyberpanel_lsi_api_abi_v1") != NULL;
}
