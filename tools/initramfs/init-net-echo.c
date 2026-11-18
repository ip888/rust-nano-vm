#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static void log_marker(const char *message)
{
	int fd = open("/dev/kmsg", O_WRONLY);
	int target = fd >= 0 ? fd : STDERR_FILENO;
	size_t length = 0;
	while (message[length])
		length++;
	(void)write(target, message, length);
	if (fd >= 0)
		(void)close(fd);
}

static int configure_interface(void)
{
	struct ifreq request = { 0 };
	struct sockaddr_in address;
	int fd = socket(AF_INET, SOCK_DGRAM, 0);
	char error[96];

	if (fd < 0) {
		(void)snprintf(error, sizeof(error), "NANOVM_NET_SOCKET_FAILED errno=%d\n", errno);
		log_marker(error);
		return -1;
	}
	(void)snprintf(request.ifr_name, IFNAMSIZ, "eth0");
	if (ioctl(fd, SIOCGIFFLAGS, &request) < 0) {
		(void)snprintf(error, sizeof(error), "NANOVM_NET_GET_FLAGS_FAILED errno=%d\n", errno);
		log_marker(error);
		goto fail;
	}
	request.ifr_flags |= IFF_UP | IFF_RUNNING;
	if (ioctl(fd, SIOCSIFFLAGS, &request) < 0) {
		(void)snprintf(error, sizeof(error), "NANOVM_NET_SET_FLAGS_FAILED errno=%d\n", errno);
		log_marker(error);
		goto fail;
	}

	inet_pton(AF_INET, "169.254.0.2", &address.sin_addr);
	address.sin_family = AF_INET;
	address.sin_port = 0;
	request.ifr_addr = *(struct sockaddr *)&address;
	if (ioctl(fd, SIOCSIFADDR, &request) < 0) {
		(void)snprintf(error, sizeof(error), "NANOVM_NET_SET_ADDR_FAILED errno=%d\n", errno);
		log_marker(error);
		goto fail;
	}

	inet_pton(AF_INET, "255.255.255.0", &address.sin_addr);
	request.ifr_netmask = *(struct sockaddr *)&address;
	if (ioctl(fd, SIOCSIFNETMASK, &request) < 0) {
		(void)snprintf(error, sizeof(error), "NANOVM_NET_SET_MASK_FAILED errno=%d\n", errno);
		log_marker(error);
		goto fail;
	}

	(void)close(fd);
	return 0;
fail:
	(void)close(fd);
	return -1;
}

static int run_echo_server(void)
{
	struct sockaddr_in address = {
		.sin_family = AF_INET,
		.sin_addr.s_addr = htonl(INADDR_ANY),
		.sin_port = htons(8080),
	};
	unsigned char buffer[4096];
	int listener = socket(AF_INET, SOCK_STREAM, 0);
	int option = 1;

	if (listener < 0)
		return -1;
	(void)setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &option, sizeof(option));
	if (bind(listener, (struct sockaddr *)&address, sizeof(address)) < 0 ||
	    listen(listener, 4) < 0) {
		(void)close(listener);
		return -1;
	}
	log_marker("NANOVM_NET_ECHO_READY\n");
	for (;;) {
		int client = accept(listener, NULL, NULL);
		ssize_t length;
		if (client < 0)
			continue;
		while ((length = read(client, buffer, sizeof(buffer))) > 0) {
			size_t written = 0;
			while (written < (size_t)length) {
				ssize_t count = write(
					client, buffer + written, (size_t)length - written);
				if (count <= 0)
					break;
				written += (size_t)count;
			}
		}
		(void)close(client);
	}
}

int main(void)
{
	log_marker("NANOVM_NET_INIT_WRAPPER_ALIVE\n");
	if (configure_interface() < 0) {
		log_marker("NANOVM_NET_INIT_CONFIG_FAILED\n");
		return 1;
	}
	if (run_echo_server() < 0) {
		log_marker("NANOVM_NET_ECHO_SERVER_FAILED\n");
		return 1;
	}
	return 0;
}
