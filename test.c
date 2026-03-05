#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <sched.h>
#include <errno.h>
#include <string.h>

int main(void)
{
    // struct sched_param param = {0};

    // if (sched_setscheduler(0, SCHED_EXT, &param) != 0) {
    //     perror("sched_setscheduler(SCHED_EXT)");
    //     return 1;
    // } 
    pid_t pid = getpid();
   

    volatile unsigned long x;

    while (1) {
        x = 0;
        for (int i = 0; i < 1000; i++) {
            x += i;
        }
        printf("Policy: %d, PID: %d\n", sched_getscheduler(pid), pid);
        printf("x = %lu\n", x);
        sleep(2);
            
    }

    return 0;
}
