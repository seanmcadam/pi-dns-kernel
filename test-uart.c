// Simple test to verify UART address
volatile unsigned int * const UART0DR = (unsigned int *)0x09000000;
volatile unsigned int * const UART0FR = (unsigned int *)0x09000018;

void kernel_main(void) {
    // Write 'A' directly to UART data register
    *UART0DR = 'A';
    *UART0DR = '\n';
    *UART0DR = '\r';
    
    while(1) {
        __asm__ volatile("wfe");
    }
}
