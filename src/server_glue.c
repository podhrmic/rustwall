/**
 * C helper file for testing whether `lib.rs` actually compiles
 */
#include "server_glue.h"
#include <pthread.h>

int tun_alloc(char *dev, int flags)
{
  struct ifreq ifr;
  int fd, err;
  char *clonedev = "/dev/net/tun";

  /* Arguments taken by the function:
   *
   * char *dev: the name of an interface (or '\0'). MUST have enough
   *   space to hold the interface name if '\0' is passed
   * int flags: interface flags (eg, IFF_TUN etc.)
   */

  /* open the clone device */
  if ((fd = open(clonedev, O_RDWR)) < 0) {
    return fd;
  }

  /* preparation of the struct ifr, of type "struct ifreq" */
  memset(&ifr, 0, sizeof(ifr));

  ifr.ifr_flags = flags; /* IFF_TUN or IFF_TAP, plus maybe IFF_NO_PI */

  if (*dev) {
    /* if a device name was specified, put it in the structure; otherwise,
     * the kernel will try to allocate the "next" device of the
     * specified type */
    strncpy(ifr.ifr_name, dev, IFNAMSIZ);
  }

  /* try to create the device */
  if ((err = ioctl(fd, TUNSETIFF, (void *) &ifr)) < 0) {
    close(fd);
    return err;
  }

  /* if the operation was successful, write back the name of the
   * interface to the variable "dev", so the caller can know
   * it. Note that the caller MUST reserve space in *dev (see calling
   * code below) */
  strcpy(dev, ifr.ifr_name);

  /*
   * Setup a non blocking call
   */
  //flags = fcntl(fd, F_GETFL, 0);
  //fcntl(fd, F_SETFL, flags | O_NONBLOCK);
  /* this is the special file descriptor that the caller will use to talk
   * with the virtual interface */
  return fd;
}

/**
 * Note: this code is normally autogenerated during seL4 build
 */
struct
{
  char content[65535];
} from_ethdriver_data;

void * ethdriver_buf = (void *) &from_ethdriver_data;

struct
{
  char content[65535];
} to_client_1_data;

void * client_buf_1 = (void *) &to_client_1_data;

void *client_buf(seL4_Word client_id)
{
  switch (client_id) {
    case 1:
      return (void *) client_buf_1;
    default:
      return NULL;
  }
}

void client_emit_1(void)
{
  printf("Client emit 1: calling seL4_signal()\n");
}

void client_emit(unsigned int badge)
{
  if (badge == 1) {
    client_emit_1();
  };
}
/**
 * END OF AUTOGENERATED CODE
 */

/**
 * Dummy version
 * Normally sends `len` data from `ethdriver_buf`
 * Returns -1 in case of an error, and probably 0 if all OK
 */
int ethdriver_tx(int len)
{
  ethdriver_init();
  //printf("C Attempt to write %i bytes\n", len);
  memcpy(tun_buffer, ethdriver_buf, len);
  len = write(tun_fd, tun_buffer, len);
  if (len < 0) {
    //perror("C Writing to interface");
    close(tun_fd);
    exit(1);
  } else {
    //printf("C Wrote %i bytes\n", len);
  }
  return 0;
}

/**
 * Dummy version
 * Normally receives `len` data and returns -1 in case of an error, and probably 0 if all OK
 */
int ethdriver_rx(int* len)
{
  ethdriver_init();

  // Note that "buffer" should be at least the MTU size of the interface, eg 1500 bytes
  timeout.tv_sec = 10;  // 5s read/write timeout
  timeout.tv_usec = 0;

  //printf("C Attemp to read\n");
  rv = select(tun_fd + 1, &set, NULL, NULL, &timeout);
  if (rv == -1) {
    perror("C select\n"); // an error accured
    return -1;
  } else {
    if (rv == 0) {
      //printf("C timeout\n"); // a timeout occured
      return -1;
    } else {
      //printf("C Reading data\n");
      *len = read(tun_fd, tun_buffer, sizeof(tun_buffer));
      //printf("C read %i bytes\n", *len);
      memcpy(ethdriver_buf, tun_buffer, *len);
      return 0;
    }
  }

/*
   printf("C Attemp to read\n");
   *len = read(tun_fd,tun_buffer,sizeof(tun_buffer));
   if(*len < 0) {
   //perror("C Reading from interface");
   //close(tun_fd);
   //exit(1);
   return -1;
   } else {
   printf("C read %i bytes\n",*len);
   memcpy(ethdriver_buf, tun_buffer, *len);
   }
   return 0;
*/
}

/**
 * Dummy version
 * Normally returns the MAC address of the ethernet driver
 */
void ethdriver_mac(uint8_t *b1, uint8_t *b2, uint8_t *b3, uint8_t *b4,
    uint8_t *b5, uint8_t *b6)
{
  static uint8_t mac[] = { 0x02, 0x00, 0x00, 0x00, 0x00, 0x01 };
  *b1 = mac[0];
  *b2 = mac[1];
  *b3 = mac[2];
  *b4 = mac[3];
  *b5 = mac[4];
  *b6 = mac[5];
}

/**
 * Main program
 */
bool ethdriver_init(void)
{
  static bool status = false;

  if (!status) {
    /* Connect to the device */
    strcpy(tun_name, "tap1");
    tun_fd = tun_alloc(tun_name, IFF_TAP | IFF_NO_PI | O_NONBLOCK); /* tun interface */

    if (tun_fd < 0) {
      perror("Allocating interface");
      exit(1);
    }

    // NON blocking read/write
    /*
    int flags = fcntl(tun_fd, F_GETFL, 0);
    fcntl(tun_fd, F_SETFL, flags | O_NONBLOCK);
    */

    FD_ZERO(&set); // clear the set
    FD_SET(tun_fd, &set); // add our file descriptor to the set

    //printf(">>ethdriver init done\n");

    status = true;
  }

  return status;
}

pthread_mutex_t mutex_ethdriver_buf = PTHREAD_MUTEX_INITIALIZER;
pthread_mutex_t mutex_client_buf = PTHREAD_MUTEX_INITIALIZER;
void ethdriver_buf_lock(void) {
  pthread_mutex_lock(&mutex_ethdriver_buf);
};
void ethdriver_buf_unlock(void) {
  pthread_mutex_unlock(&mutex_ethdriver_buf);
};
void client_buf_lock(void) {
  pthread_mutex_lock(&mutex_client_buf);
};
void client_buf_unlock(void) {
  pthread_mutex_unlock(&mutex_client_buf);
};
