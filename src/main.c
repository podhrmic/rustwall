/**
 * C helper file for testing whether `lib.rs` actually compiles
 */
#include <stdio.h>
#include <stdint.h>

/*
// Stubs for the external firewall calls
void packet_in(uint32_t src_addr, uint16_t src_port,
            uint32_t dest_addr, uint16_t dest_port,
      uint16_t payload_len, void *payload);

void packet_out(uint32_t src_addr, uint16_t src_port,
              uint32_t dest_addr, uint16_t dest_port,
          uint16_t payload_len, void *payload);
*/


/**
 * A helper define to make this look more like an actual seL4 file
 */
typedef uint32_t seL4_Word;

extern void client_mac(uint8_t *b1, uint8_t *b2, uint8_t *b3, uint8_t *b4, uint8_t *b5, uint8_t *b6);
extern int client_tx(int len);
extern int client_rx(int *len);
extern void ethdriver_has_data_callback(seL4_Word badge);

/**
 * Dummy version
 * Normally sends `len` data from `ethdriver_buf`
 * Returns -1 in case of an error, and probably 0 if all OK
 */
int ethdriver_tx(int len) {
  (void)len;
  return 0;
}

/**
 * Dummy version
 * Normally receives `len` data and returns -1 in case of an error, and probably 0 if all OK
 */
int ethdriver_rx(int* len) {
  *len = 42;
  return 0;
}

/**
 * Dummy version
 * Normally returns the MAC address of the ethernet driver
 */
void ethdriver_mac(uint8_t *b1, uint8_t *b2, uint8_t *b3, uint8_t *b4, uint8_t *b5, uint8_t *b6) {
  printf("Hello from ethdriver_mac: ");
  printf("b1=%u, ", *b1);
  printf("b2=%u, ", *b2);
  printf("b3=%u, ", *b3);
  printf("b4=%u, ", *b4);
  printf("b5=%u, ", *b5);
  printf("b6=%u\n", *b6);
}


/**
 * Note: this code is normally autogenerated during seL4 build
 */
struct {
  char content[4096];
} from_ethdriver_data;

volatile void * ethdriver_buf = (volatile void *) & from_ethdriver_data;


struct {
   char content[4096];
} to_client_1_data;

volatile void * client_buf_1 = (volatile void *) & to_client_1_data;

void *client_buf(seL4_Word client_id) {
  switch (client_id) {
    case 1:
      return (void *) client_buf_1;
    default:
      return NULL;
  }
}

void client_emit_1(void) {
  printf("Client emit 1: calling seL4_signal()\n");
}


void client_emit(unsigned int badge) {
  // here is normally a array of functions:
  //static void (*lookup[])(void) {
  //  [1] = client_emit_1,
  //};
  //lookup[badge]();
  if (badge == 1) {
    client_emit_1();
  };
}
/**
 * END OF AUTOGENERATED CODE
 */

/**
 * Main program
 */
int main() {
  printf("hello from C\n");

  uint8_t b1 = 11;
  uint8_t b2 = 22;
  uint8_t b3 = 33;
  uint8_t b4 = 44;
  uint8_t b5 = 55;
  uint8_t b6 = 66;

  client_mac(&b1, &b2, &b3, &b4, &b5, &b6);

  int len = 32;
  printf("client_tx returned %u bytes\n", client_tx(len));

  int returnval = client_rx(&len);
  printf("client_rx received %u bytes with return value %i\n", len, returnval);

  ethdriver_has_data_callback(66);

  printf("done\n");
}
