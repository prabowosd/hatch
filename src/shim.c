/* g_api is a thread-local exported by the server; stable Rust cannot read an extern TLS symbol. */
#include <ls.h>

const lsi_api_t *hatch_api(void) { return g_api; }
