/* Rootless capability lookup only: does not enumerate the volume. */
#include <sys/attr.h>
#include <sys/mount.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    struct statfs fs;
    if (statfs(argv[1], &fs)) { perror("statfs"); return 1; }
    struct attrlist request = {0};
    request.bitmapcount = ATTR_BIT_MAP_COUNT;
    request.volattr = ATTR_VOL_CAPABILITIES;
    struct { unsigned int length; vol_capabilities_attr_t caps; } response = {0};
    if (getattrlist(argv[1], &request, &response, sizeof(response), 0)) {
        perror("getattrlist capabilities"); return 1;
    }
    unsigned int valid = response.caps.valid[VOL_CAPABILITIES_INTERFACES];
    unsigned int supported = response.caps.capabilities[VOL_CAPABILITIES_INTERFACES] & valid;
    printf("filesystem=%s block_size=%u source=%s\n", fs.f_fstypename, fs.f_bsize, fs.f_mntfromname);
    printf("searchfs_known=%d searchfs_supported=%d\n", !!(valid & VOL_CAP_INT_SEARCHFS), !!(supported & VOL_CAP_INT_SEARCHFS));
    /* READDIRATTR describes getdirentriesattr, not getattrlistbulk. */
    printf("getdirentriesattr_known=%d getdirentriesattr_supported=%d\n", !!(valid & VOL_CAP_INT_READDIRATTR), !!(supported & VOL_CAP_INT_READDIRATTR));
    return 0;
}
