CC = gcc
CFLAGS = -Wall -Wextra -std=gnu11 -O2
TARGET = test-syscalls

# Source files
SRCS = main.c \
       test-openat.c \
       test-read.c \
       test-write.c \
       test-close.c \
       test-dup.c \
       test-dup2.c \
       test-stat.c \
       test-fstat.c \
       test-lstat.c \
       test-getdents64.c \
       test-append.c \
       test-pread-sparse.c \
       test-link.c \
       test-unlink.c \
       test-copyup-inode-stability.c \
       test-rename.c \
       test-mknod.c \
       test-mkfifo.c

# Object files
OBJS = $(SRCS:.c=.o)

# Default target
all: $(TARGET)

# Link the executable
$(TARGET): $(OBJS)
	$(CC) $(CFLAGS) -o $(TARGET) $(OBJS)

# Compile source files
%.o: %.c test-common.h
	$(CC) $(CFLAGS) -c $< -o $@

# Clean build artifacts
clean:
	rm -f $(TARGET) $(OBJS)

# Phony targets
.PHONY: all clean
