#define _GNU_SOURCE
#include "test-common.h"
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <fcntl.h>
#include <unistd.h>

int test_mknod(const char *base_path) {
    char path[512];
    struct stat st;
    int result;

    /* Test 1: Create a FIFO (named pipe) using mknod */
    snprintf(path, sizeof(path), "%s/test_fifo_mknod", base_path);
    unlink(path);  /* Clean up any previous test file */

    result = mknod(path, S_IFIFO | 0644, 0);
    TEST_ASSERT_ERRNO(result == 0, "mknod FIFO creation should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT(S_ISFIFO(st.st_mode), "created node should be a FIFO");
    TEST_ASSERT((st.st_mode & 0777) == 0644, "FIFO should have correct permissions");

    unlink(path);

    /* Test 2: Create a regular file using mknod */
    snprintf(path, sizeof(path), "%s/test_file_mknod", base_path);
    unlink(path);

    result = mknod(path, S_IFREG | 0600, 0);
    TEST_ASSERT_ERRNO(result == 0, "mknod regular file creation should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on regular file should succeed");
    TEST_ASSERT(S_ISREG(st.st_mode), "created node should be a regular file");
    TEST_ASSERT((st.st_mode & 0777) == 0600, "regular file should have correct permissions");

    unlink(path);

    /* Test 3: mknod with existing path should fail with EEXIST */
    snprintf(path, sizeof(path), "%s/test.txt", base_path);

    result = mknod(path, S_IFREG | 0644, 0);
    TEST_ASSERT(result < 0, "mknod on existing file should fail");
    TEST_ASSERT(errno == EEXIST, "errno should be EEXIST for existing path");

    /* Test 4: mknod in non-existent directory should fail with ENOENT */
    snprintf(path, sizeof(path), "%s/nonexistent_dir/test_mknod", base_path);

    result = mknod(path, S_IFREG | 0644, 0);
    TEST_ASSERT(result < 0, "mknod in non-existent directory should fail");
    TEST_ASSERT(errno == ENOENT, "errno should be ENOENT for non-existent directory");

    /* Test 5: Create a FIFO with different permissions */
    snprintf(path, sizeof(path), "%s/test_fifo_perms", base_path);
    unlink(path);

    result = mknod(path, S_IFIFO | 0755, 0);
    TEST_ASSERT_ERRNO(result == 0, "mknod FIFO with 0755 should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO should succeed");
    TEST_ASSERT((st.st_mode & 0777) == 0755, "FIFO should have 0755 permissions");

    unlink(path);

    /* Test 6: Create FIFO in subdirectory */
    char subdir_path[512];
    snprintf(subdir_path, sizeof(subdir_path), "%s/subdir", base_path);
    mkdir(subdir_path, 0755);

    snprintf(path, sizeof(path), "%s/subdir/test_fifo_subdir", base_path);
    unlink(path);

    result = mknod(path, S_IFIFO | 0644, 0);
    TEST_ASSERT_ERRNO(result == 0, "mknod FIFO in subdirectory should succeed");

    result = stat(path, &st);
    TEST_ASSERT_ERRNO(result == 0, "stat on FIFO in subdirectory should succeed");
    TEST_ASSERT(S_ISFIFO(st.st_mode), "created node in subdir should be a FIFO");

    unlink(path);

    /* Test 7: Character device creation (may require CAP_MKNOD) */
    snprintf(path, sizeof(path), "%s/test_chrdev", base_path);
    unlink(path);

    result = mknod(path, S_IFCHR | 0666, makedev(1, 3));  /* /dev/null major/minor */
    if (result < 0 && (errno == EPERM || errno == EACCES)) {
        printf("  (Skipping device node tests - requires CAP_MKNOD)\n");
    } else if (result == 0) {
        result = stat(path, &st);
        TEST_ASSERT_ERRNO(result == 0, "stat on character device should succeed");
        TEST_ASSERT(S_ISCHR(st.st_mode), "created node should be a character device");
        TEST_ASSERT(major(st.st_rdev) == 1, "character device should have correct major number");
        TEST_ASSERT(minor(st.st_rdev) == 3, "character device should have correct minor number");
        unlink(path);
    }

    /* Test 8: Block device creation (may require CAP_MKNOD) */
    snprintf(path, sizeof(path), "%s/test_blkdev", base_path);
    unlink(path);

    result = mknod(path, S_IFBLK | 0666, makedev(7, 0));  /* loop0 major/minor */
    if (result < 0 && (errno == EPERM || errno == EACCES)) {
        /* Expected without CAP_MKNOD - already noted above */
    } else if (result == 0) {
        result = stat(path, &st);
        TEST_ASSERT_ERRNO(result == 0, "stat on block device should succeed");
        TEST_ASSERT(S_ISBLK(st.st_mode), "created node should be a block device");
        TEST_ASSERT(major(st.st_rdev) == 7, "block device should have correct major number");
        TEST_ASSERT(minor(st.st_rdev) == 0, "block device should have correct minor number");
        unlink(path);
    }

    /* Test 9: Socket creation via mknod is typically not supported */
    snprintf(path, sizeof(path), "%s/test_socket_mknod", base_path);
    unlink(path);

    result = mknod(path, S_IFSOCK | 0644, 0);
    /* Socket creation via mknod may fail with EOPNOTSUPP or succeed depending on FS */
    if (result == 0) {
        result = stat(path, &st);
        TEST_ASSERT_ERRNO(result == 0, "stat on socket should succeed");
        TEST_ASSERT(S_ISSOCK(st.st_mode), "created node should be a socket");
        unlink(path);
    } else {
        /* EOPNOTSUPP, EPERM, or other errors are acceptable for sockets */
        printf("  (Socket creation via mknod returned errno=%d: %s - this is acceptable)\n",
               errno, strerror(errno));
    }

    /* Test 10: Empty filename should fail */
    snprintf(path, sizeof(path), "%s/", base_path);
    result = mknod(path, S_IFREG | 0644, 0);
    TEST_ASSERT(result < 0, "mknod with empty filename should fail");

    return 0;
}
