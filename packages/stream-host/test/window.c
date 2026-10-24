#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
int main(void) {
  Display *d = XOpenDisplay(NULL);
  if (!d) return 2;
  int s = DefaultScreen(d), count = 0;
  Window w = XCreateSimpleWindow(d, RootWindow(d, s), 0, 0, 640, 360, 0, 0, 0x174063);
  XSelectInput(d, w, ExposureMask | ButtonPressMask | KeyPressMask);
  XMapWindow(d, w);
  XSetInputFocus(d, w, RevertToParent, CurrentTime);
  GC gc = XCreateGC(d, w, 0, NULL);
  for (;;) {
    XEvent e; XNextEvent(d, &e);
    if (e.type == ButtonPress || e.type == KeyPress) count++;
    XSetForeground(d, gc, count % 2 ? 0xe95821 : 0x174063);
    XFillRectangle(d, w, gc, 0, 0, 640, 360);
    XSetForeground(d, gc, 0xffffff);
    char text[128]; snprintf(text, sizeof(text), "Enginebox native input count: %d", count);
    XDrawString(d, w, gc, 20, 40, text, (int)__builtin_strlen(text));
    XFlush(d);
  }
}
