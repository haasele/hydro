# Hydro-native emulation of CPython's `turtle` module (3.14 API shape).
#
# CPython's `Lib/turtle.py` builds its module-level wrappers by generating
# source at import time and requires `tkinter` then too.  Hydro has no runtime
# compiler and this build does not ship `tkinter`, so this module implements
# the public API directly in Python:
#
#   * geometry, heading/mode bookkeeping and turtle state are fully real;
#   * a Tk root is only attempted when a screen is actually created, and if
#     `tkinter`/a DISPLAY is unavailable the screen falls back to a real
#     in-memory canvas (`_HeadlessCanvas`) that records every item;
#   * event binding and timers either bind real Tk callbacks or raise
#     `NotImplementedError`; they never pretend an event fired.
#
# Copyright (c) 2001 Python Software Foundation; All Rights Reserved.
# Licensed under the PSF License Version 2 -- see stdlib/LICENSE.
import math


_CFG = {
    "width": 800,
    "height": 600,
    "canvwidth": 400,
    "canvheight": 300,
    "mode": "standard",
    "colormode": 1.0,
    "delay": 10,
    "undobuffersize": 1000,
    "shape": "classic",
    "pencolor": "black",
    "fillcolor": "black",
    "resizemode": "noresize",
    "visible": True,
    "title": "Python Turtle Graphics",
}


class TurtleGraphicsError(Exception):
    """Some TurtleGraphics Error."""


class Terminator(Exception):
    """Raised when turtle graphics is used after its screen was closed."""


class _Registry:
    """Module-level state.

    Hydro does not propagate plain ``global`` assignments back to the module
    namespace, so the default screen and turtle live on a class instead.
    """

    screen = None
    pen = None


# ---------------------------------------------------------------------------
# Vector helper
# ---------------------------------------------------------------------------


class Vec2D(object):
    """A two-dimensional vector used by turtle geometry.

    Mirrors CPython's ``Vec2D`` (which is a tuple subclass) with the same
    arithmetic: ``a + b``, ``a - b``, ``a * b`` (inner product), ``k * a``,
    ``abs(a)`` and ``a.rotate(angle)``.
    """

    def __init__(self, x, y):
        self._x = x
        self._y = y

    @property
    def x(self):
        return self._x

    @property
    def y(self):
        return self._y

    def __len__(self):
        return 2

    def __getitem__(self, index):
        if index == 0:
            return self._x
        if index == 1:
            return self._y
        if index == -1:
            return self._y
        if index == -2:
            return self._x
        raise IndexError("Vec2D index out of range")

    def __iter__(self):
        yield self._x
        yield self._y

    def __eq__(self, other):
        try:
            return self._x == other[0] and self._y == other[1]
        except (TypeError, IndexError, KeyError):
            return False

    def __ne__(self, other):
        return not self.__eq__(other)

    def __hash__(self):
        return hash(self._x) ^ hash(self._y)

    def __repr__(self):
        return "(%.2f,%.2f)" % (self._x, self._y)

    def __str__(self):
        return "(%.2f,%.2f)" % (self._x, self._y)

    def __add__(self, other):
        return Vec2D(self._x + other[0], self._y + other[1])

    def __radd__(self, other):
        return Vec2D(other[0] + self._x, other[1] + self._y)

    def __sub__(self, other):
        return Vec2D(self._x - other[0], self._y - other[1])

    def __rsub__(self, other):
        return Vec2D(other[0] - self._x, other[1] - self._y)

    def __mul__(self, other):
        if isinstance(other, Vec2D):
            return self._x * other[0] + self._y * other[1]
        return Vec2D(self._x * other, self._y * other)

    def __rmul__(self, other):
        if isinstance(other, (int, float)):
            return Vec2D(self._x * other, self._y * other)
        return NotImplemented

    def __truediv__(self, other):
        return Vec2D(self._x / other, self._y / other)

    def __neg__(self):
        return Vec2D(-self._x, -self._y)

    def __abs__(self):
        return math.hypot(self._x, self._y)

    # Hydro does not dispatch user-class arithmetic operators yet, so the
    # turtle internals use these explicit helpers (the dunder methods above
    # remain for CPython-compatible user code).
    def add(self, other):
        return Vec2D(self._x + other[0], self._y + other[1])

    def sub(self, other):
        return Vec2D(self._x - other[0], self._y - other[1])

    def scaled(self, factor):
        return Vec2D(self._x * factor, self._y * factor)

    def dot(self, other):
        return self._x * other[0] + self._y * other[1]

    def norm(self):
        return math.hypot(self._x, self._y)

    def rotate(self, angle):
        """Rotate the vector counterclockwise by *angle* degrees."""
        rad = math.radians(angle)
        c = math.cos(rad)
        s = math.sin(rad)
        return Vec2D(self._x * c - self._y * s,
                     self._x * s + self._y * c)


# ---------------------------------------------------------------------------
# Colours
# ---------------------------------------------------------------------------


_COLOR_NAMES = {
    "aliceblue", "antiquewhite", "aqua", "aquamarine", "azure", "beige",
    "bisque", "black", "blanchedalmond", "blue", "blueviolet", "brown",
    "burlywood", "cadetblue", "chartreuse", "chocolate", "coral",
    "cornflowerblue", "cornsilk", "crimson", "cyan", "darkblue", "darkcyan",
    "darkgoldenrod", "darkgray", "darkgreen", "darkgrey", "darkkhaki",
    "darkmagenta", "darkolivegreen", "darkorange", "darkorchid", "darkred",
    "darksalmon", "darkseagreen", "darkslateblue", "darkslategray",
    "darkslategrey", "darkturquoise", "darkviolet", "deeppink", "deepskyblue",
    "dimgray", "dimgrey", "dodgerblue", "firebrick", "floralwhite",
    "forestgreen", "fuchsia", "gainsboro", "ghostwhite", "gold", "goldenrod",
    "gray", "green", "greenyellow", "grey", "honeydew", "hotpink",
    "indianred", "indigo", "ivory", "khaki", "lavender", "lavenderblush",
    "lawngreen", "lemonchiffon", "lightblue", "lightcoral", "lightcyan",
    "lightgoldenrodyellow", "lightgray", "lightgreen", "lightgrey",
    "lightpink", "lightsalmon", "lightseagreen", "lightskyblue",
    "lightslategray", "lightslategrey", "lightsteelblue", "lightyellow",
    "lime", "limegreen", "linen", "magenta", "maroon", "mediumaquamarine",
    "mediumblue", "mediumorchid", "mediumpurple", "mediumseagreen",
    "mediumslateblue", "mediumspringgreen", "mediumturquoise",
    "mediumvioletred", "midnightblue", "mintcream", "mistyrose", "moccasin",
    "navajowhite", "navy", "oldlace", "olive", "olivedrab", "orange",
    "orangered", "orchid", "palegoldenrod", "palegreen", "paleturquoise",
    "palevioletred", "papayawhip", "peachpuff", "peru", "pink", "plum",
    "powderblue", "purple", "red", "rosybrown", "royalblue", "saddlebrown",
    "salmon", "sandybrown", "seagreen", "seashell", "sienna", "silver",
    "skyblue", "slateblue", "slategray", "slategrey", "snow", "springgreen",
    "steelblue", "tan", "teal", "thistle", "tomato", "turquoise", "violet",
    "wheat", "white", "whitesmoke", "yellow", "yellowgreen",
}


def _iscolorstring(color):
    if color == "":
        return True
    if color.startswith("#"):
        if len(color) == 4 or len(color) == 7:
            for ch in color[1:]:
                if ch not in "0123456789abcdefABCDEF":
                    return False
            return True
        return False
    if not isinstance(color, str):
        return False
    return color.lower() in _COLOR_NAMES


# ---------------------------------------------------------------------------
# Shape
# ---------------------------------------------------------------------------


class Shape(object):
    """Data structure for the turtle shapes."""

    def __init__(self, type_, data=None):
        self._type = type_
        if type_ == "polygon":
            if isinstance(data, list):
                data = tuple(data)
            self._data = data
        elif type_ == "image":
            self._data = data
        elif type_ == "compound":
            self._data = []
            if data is not None:
                for component in data:
                    self.addcomponent(component[0], component[1],
                                      component[2])
        else:
            raise TurtleGraphicsError("invalid shape type: %s" % type_)

    def addcomponent(self, poly, fill, outline=None):
        if self._type != "compound":
            raise TurtleGraphicsError("cannot add component to non-compound shape")
        if isinstance(poly, list):
            poly = tuple(poly)
        if outline is None:
            outline = fill
        self._data.append((poly, fill, outline))

    def __repr__(self):
        return "<Shape %s>" % self._type

    def __str__(self):
        return "<Shape %s>" % self._type


# ---------------------------------------------------------------------------
# Canvas back ends
# ---------------------------------------------------------------------------


class _HeadlessCanvas(object):
    """A real in-memory canvas used when Tk cannot be created.

    Items are stored with their type, coordinates and options so drawing,
    deletion, item configuration and introspection remain meaningful without
    a display.
    """

    def __init__(self, width=500, height=350, bg="white"):
        self.width = int(width)
        self.height = int(height)
        self.canvwidth = int(width)
        self.canvheight = int(height)
        self.bg = bg
        self._items = {}
        self._order = []
        self._next_id = 1

    def _new_item(self, kind, coords, options):
        item = self._next_id
        self._next_id += 1
        self._items[item] = {"type": kind, "coords": coords, "options": options}
        self._order.append(item)
        return item

    def _flatten(self, args):
        coords = []
        for arg in args:
            if isinstance(arg, (list, tuple)):
                for value in arg:
                    if isinstance(value, (list, tuple)):
                        for inner in value:
                            coords.append(inner)
                    else:
                        coords.append(value)
            else:
                coords.append(arg)
        return coords

    def create_line(self, *args, **kw):
        return self._new_item("line", self._flatten(args), dict(kw))

    def create_polygon(self, *args, **kw):
        return self._new_item("polygon", self._flatten(args), dict(kw))

    def create_oval(self, *args, **kw):
        return self._new_item("oval", self._flatten(args), dict(kw))

    def create_text(self, *args, **kw):
        coords = self._flatten(args)
        return self._new_item("text", coords, dict(kw))

    def create_image(self, *args, **kw):
        return self._new_item("image", self._flatten(args), dict(kw))

    def delete(self, *items):
        for item in items:
            if item == "all":
                self._items = {}
                self._order = []
            else:
                if item in self._items:
                    del self._items[item]
                if item in self._order:
                    self._order.remove(item)

    def itemconfig(self, item, **kw):
        if item in self._items:
            self._items[item]["options"].update(kw)

    config = itemconfig

    def itemcget(self, item, option):
        if item in self._items:
            return self._items[item]["options"].get(option, "")
        raise KeyError(item)

    def coords(self, item, *args):
        if args:
            if item in self._items:
                self._items[item]["coords"] = self._flatten(args)
            return None
        if item in self._items:
            return list(self._items[item]["coords"])
        return []

    def type(self, item):
        if item in self._items:
            return self._items[item]["type"]
        return ""

    def find_all(self):
        return list(self._order)

    def configure(self, **kw):
        if "bg" in kw:
            self.bg = kw["bg"]
        if "background" in kw:
            self.bg = kw["background"]
        if "width" in kw:
            self.width = int(kw["width"])
        if "height" in kw:
            self.height = int(kw["height"])

    config = configure

    def cget(self, option):
        if option == "bg" or option == "background":
            return self.bg
        if option == "width":
            return self.width
        if option == "height":
            return self.height
        return ""

    def winfo_width(self):
        return self.width

    def winfo_height(self):
        return self.height

    def update(self):
        return None

    def update_idletasks(self):
        return None

    def reset(self, canvwidth=None, canvheight=None, bg=None):
        if canvwidth is not None:
            self.canvwidth = int(canvwidth)
        if canvheight is not None:
            self.canvheight = int(canvheight)
        if bg is not None:
            self.bg = bg
        return None

    def adjustScrolls(self):
        return None

    def set_window_size(self, width, height):
        self.width = int(width)
        self.height = int(height)
        self.configure(width=self.width, height=self.height)

    def postscript(self):
        raise NotImplementedError(
            "postscript() requires a Tk canvas; this build has no tkinter"
        )


class _TkCanvasAdapter(object):
    """Adapter exposing turtle coordinates over a real Tk canvas."""

    def __init__(self, canvas, width, height):
        self._canvas = canvas
        self._width = int(width)
        self._height = int(height)
        self.width = int(width)
        self.height = int(height)
        self.canvwidth = int(width)
        self.canvheight = int(height)

    def _xy(self, x, y):
        return (self._width / 2.0 + x, self._height / 2.0 - y)

    def _transform(self, coords):
        out = []
        i = 0
        length = len(coords)
        while i + 1 < length:
            xy = self._xy(coords[i], coords[i + 1])
            out.append(xy[0])
            out.append(xy[1])
            i += 2
        return out

    def _flatten(self, args):
        coords = []
        for arg in args:
            if isinstance(arg, (list, tuple)):
                for value in arg:
                    if isinstance(value, (list, tuple)):
                        for inner in value:
                            coords.append(inner)
                    else:
                        coords.append(value)
            else:
                coords.append(arg)
        return coords

    def create_line(self, *args, **kw):
        coords = self._transform(self._flatten(args))
        fill = kw.pop("fill", "black")
        width = kw.pop("width", 1)
        return self._canvas.create_line(*coords, fill=fill, width=width,
                                        capstyle="round")

    def create_polygon(self, *args, **kw):
        coords = self._transform(self._flatten(args))
        fill = kw.pop("fill", "")
        outline = kw.pop("outline", "black")
        width = kw.pop("width", 1)
        return self._canvas.create_polygon(*coords, fill=fill,
                                           outline=outline, width=width)

    def create_oval(self, *args, **kw):
        coords = self._transform(self._flatten(args))
        fill = kw.pop("fill", "black")
        outline = kw.pop("outline", "black")
        return self._canvas.create_oval(*coords, fill=fill, outline=outline)

    def create_text(self, x, y, **kw):
        xy = self._xy(x, y)
        return self._canvas.create_text(xy[0], xy[1], **kw)

    def create_image(self, x, y, **kw):
        xy = self._xy(x, y)
        return self._canvas.create_image(xy[0], xy[1], **kw)

    def delete(self, *items):
        self._canvas.delete(*items)

    def itemconfig(self, item, **kw):
        self._canvas.itemconfig(item, **kw)

    config = itemconfig

    def itemcget(self, item, option):
        return self._canvas.itemcget(item, option)

    def coords(self, item, *args):
        if args:
            return self._canvas.coords(item, *self._transform(self._flatten(args)))
        return self._canvas.coords(item)

    def type(self, item):
        return self._canvas.type(item)

    def find_all(self):
        return self._canvas.find_all()

    def configure(self, **kw):
        self._canvas.configure(**kw)

    config = configure

    def cget(self, option):
        return self._canvas.cget(option)

    def winfo_width(self):
        return self._canvas.winfo_width()

    def winfo_height(self):
        return self._canvas.winfo_height()

    def update(self):
        return self._canvas.update()

    def update_idletasks(self):
        return self._canvas.update_idletasks()

    def reset(self, canvwidth=None, canvheight=None, bg=None):
        if canvwidth is not None:
            self.canvwidth = int(canvwidth)
        if canvheight is not None:
            self.canvheight = int(canvheight)
        options = {"scrollregion": (-self.canvwidth // 2,
                                    -self.canvheight // 2,
                                    self.canvwidth // 2,
                                    self.canvheight // 2)}
        if bg is not None:
            options["bg"] = bg
        self._canvas.configure(**options)
        return None

    def adjustScrolls(self):
        self.reset()
        return None

    def set_window_size(self, width, height):
        self.width = int(width)
        self.height = int(height)
        self._width = self.width
        self._height = self.height
        self._canvas.configure(width=self.width, height=self.height)

    def postscript(self):
        return self._canvas.postscript()

    def bind(self, sequence, func, add=None):
        return self._canvas.bind(sequence, func, add)

    def tag_bind(self, item, sequence, func, add=None):
        return self._canvas.tag_bind(item, sequence, func, add)

    def focus_set(self):
        return self._canvas.focus_set()

    def configure_bg(self, color):
        self._canvas.configure(bg=color)


class ScrolledCanvas(object):
    """Canvas wrapper with scroll metadata.

    When a Tk master is supplied a real ``tkinter.Canvas`` (inside a frame
    with scrollbars) is used; otherwise the same API is backed by
    :class:`_HeadlessCanvas`.
    """

    def __init__(self, master=None, width=500, height=350,
                 canvwidth=600, canvheight=500):
        self.master = master
        self.width = int(width)
        self.height = int(height)
        self.canvwidth = int(canvwidth)
        self.canvheight = int(canvheight)
        self.bg = "white"
        self._rootwindow = None
        self._frame = None
        self._rawcanvas = None
        self._tk_ok = False
        self._target = None

        if master is not None:
            try:
                import tkinter
                frame = tkinter.Frame(master, width=self.width,
                                      height=self.height)
                frame.pack(fill="both", expand=True)
                raw = tkinter.Canvas(frame, width=self.width,
                                     height=self.height, bg=self.bg,
                                     highlightthickness=0)
                raw.pack(fill="both", expand=True)
                self._frame = frame
                self._rawcanvas = raw
                self._rootwindow = master.winfo_toplevel()
                self._tk_ok = True
            except Exception:
                self._frame = None
                self._rawcanvas = None
                self._rootwindow = None
                self._tk_ok = False

        if self._tk_ok:
            self._target = _TkCanvasAdapter(self._rawcanvas,
                                            self.width, self.height)
        else:
            self._target = _HeadlessCanvas(self.width, self.height, self.bg)

    # -- item API ---------------------------------------------------------
    def create_line(self, *args, **kw):
        return self._target.create_line(*args, **kw)

    def create_polygon(self, *args, **kw):
        return self._target.create_polygon(*args, **kw)

    def create_oval(self, *args, **kw):
        return self._target.create_oval(*args, **kw)

    def create_text(self, *args, **kw):
        return self._target.create_text(*args, **kw)

    def create_image(self, *args, **kw):
        return self._target.create_image(*args, **kw)

    def delete(self, *items):
        return self._target.delete(*items)

    def itemconfig(self, item, **kw):
        return self._target.itemconfig(item, **kw)

    config = itemconfig

    def itemcget(self, item, option):
        return self._target.itemcget(item, option)

    def coords(self, item, *args):
        return self._target.coords(item, *args)

    def type(self, item):
        return self._target.type(item)

    def find_all(self):
        return self._target.find_all()

    def configure(self, **kw):
        if "bg" in kw:
            self.bg = kw["bg"]
        if "background" in kw:
            self.bg = kw["background"]
        if "width" in kw:
            self.width = int(kw["width"])
        if "height" in kw:
            self.height = int(kw["height"])
        return self._target.configure(**kw)

    config = configure

    def cget(self, option):
        return self._target.cget(option)

    def winfo_width(self):
        return self._target.winfo_width()

    def winfo_height(self):
        return self._target.winfo_height()

    def update(self):
        return self._target.update()

    def update_idletasks(self):
        return self._target.update_idletasks()

    def postscript(self):
        return self._target.postscript()

    def bind(self, sequence, func, add=None):
        if not self._tk_ok:
            raise NotImplementedError(
                "event binding requires tkinter; this Hydro build has no Tk screen"
            )
        return self._target.bind(sequence, func, add)

    def tag_bind(self, item, sequence, func, add=None):
        if not self._tk_ok:
            raise NotImplementedError(
                "event binding requires tkinter; this Hydro build has no Tk screen"
            )
        return self._target.tag_bind(item, sequence, func, add)

    def focus_set(self):
        if not self._tk_ok:
            raise NotImplementedError(
                "focus_set() requires tkinter; this Hydro build has no Tk screen"
            )
        return self._target.focus_set()

    def set_window_size(self, width, height):
        self.width = int(width)
        self.height = int(height)
        if self._tk_ok:
            self._target._width = self.width
            self._target._height = self.height
            self._rawcanvas.configure(width=self.width,
                                      height=self.height)
        else:
            self._target.configure(width=self.width, height=self.height)

    def reset(self, canvwidth=None, canvheight=None, bg=None):
        if canvwidth is not None:
            self.canvwidth = int(canvwidth)
        if canvheight is not None:
            self.canvheight = int(canvheight)
        if bg is not None:
            self.bg = bg
        self._target.reset(canvwidth, canvheight, bg)

    def adjustScrolls(self):
        if self._tk_ok:
            self._rawcanvas.configure(
                scrollregion=(-self.canvwidth // 2, -self.canvheight // 2,
                              self.canvwidth // 2, self.canvheight // 2))
            self._rawcanvas.xview_moveto(0.5)
            self._rawcanvas.yview_moveto(0.5)
        return None


# ---------------------------------------------------------------------------
# TurtleScreen
# ---------------------------------------------------------------------------


class TurtleScreen(object):
    """Provides screen-oriented drawing methods.

    A screen can be backed by a real Tk canvas (when Tk can be created) or by
    an in-memory canvas.  Constructing a screen is the only point at which a
    Tk root is attempted; importing the module never creates one.
    """

    _RUNNING = True

    def __init__(self, cv=None, mode=_CFG["mode"],
                 colormode=_CFG["colormode"], delay=_CFG["delay"],
                 width=None, height=None):
        if width is None:
            width = _CFG["width"]
        if height is None:
            height = _CFG["height"]
        self._root = None
        self._tk_ok = False

        if cv is None:
            root = None
            try:
                import tkinter
                root = tkinter.Tk()
            except Exception:
                root = None
            if root is not None:
                try:
                    self.cv = ScrolledCanvas(root, width, height,
                                             _CFG["canvwidth"],
                                             _CFG["canvheight"])
                    self._root = root
                    self._tk_ok = getattr(self.cv, "_tk_ok", False)
                    if self._tk_ok:
                        root.title(_CFG["title"])
                        root.update_idletasks()
                    else:
                        try:
                            root.destroy()
                        except Exception:
                            pass
                        self._root = None
                except Exception:
                    try:
                        root.destroy()
                    except Exception:
                        pass
                    self._root = None
                    self.cv = ScrolledCanvas(None, width, height,
                                             _CFG["canvwidth"],
                                             _CFG["canvheight"])
            else:
                self.cv = ScrolledCanvas(None, width, height,
                                         _CFG["canvwidth"],
                                         _CFG["canvheight"])
        elif isinstance(cv, ScrolledCanvas):
            self.cv = cv
            self._root = cv._rootwindow
            self._tk_ok = cv._tk_ok
        elif (hasattr(cv, "create_line") and hasattr(cv, "winfo_width")
                and not isinstance(cv, _HeadlessCanvas)):
            # A raw tkinter Canvas: wrap it so turtle coordinates keep their
            # mathematical orientation (y grows upwards).
            width = cv.winfo_width()
            height = cv.winfo_height()
            if width is None or width <= 1:
                width = _CFG["width"]
            if height is None or height <= 1:
                height = _CFG["height"]
            self.cv = _TkCanvasAdapter(cv, width, height)
            self._root = cv.winfo_toplevel()
            self._tk_ok = True
        else:
            self.cv = cv
            self._root = None
            self._tk_ok = False

        self._mode = mode
        self._colormode = colormode
        self._delayvalue = int(delay)
        self._tracing = 1
        self._updatecounter = 0
        self._bgcolor = "white"
        self._bgpicname = "nopic"
        self._keys = []
        self._turtles = []
        self._title = _CFG["title"]
        self.xscale = 1.0
        self.yscale = 1.0
        self._shapes = {
            "arrow": Shape("polygon", ((-10, 0), (10, 0), (0, 10))),
            "turtle": Shape("polygon", ((0, 16), (-2, 14), (-1, 10), (-4, 7),
                                        (-7, 9), (-9, 8), (-6, 5), (-7, 1),
                                        (-5, -3), (-8, -6), (-6, -8),
                                        (-4, -5), (0, -7), (4, -5), (6, -8),
                                        (8, -6), (5, -3), (7, 1), (6, 5),
                                        (9, 8), (7, 9), (4, 7), (1, 10),
                                        (2, 14))),
            "circle": Shape("polygon", ((10, 0), (9.51, 3.09), (8.09, 5.88),
                                        (5.88, 8.09), (3.09, 9.51), (0, 10),
                                        (-3.09, 9.51), (-5.88, 8.09),
                                        (-8.09, 5.88), (-9.51, 3.09),
                                        (-10, 0), (-9.51, -3.09),
                                        (-8.09, -5.88), (-5.88, -8.09),
                                        (-3.09, -9.51), (0, -10),
                                        (3.09, -9.51), (5.88, -8.09),
                                        (8.09, -5.88), (9.51, -3.09))),
            "square": Shape("polygon", ((10, -10), (10, 10), (-10, 10),
                                        (-10, -10))),
            "triangle": Shape("polygon", ((10, -5.77), (0, 11.55),
                                          (-10, -5.77))),
            "classic": Shape("polygon", ((0, 0), (-5, -9), (0, -7), (5, -9))),
            "blank": Shape("image", ""),
        }
        self.bgcolor("white")
        self._tracers = 1

    # -- colour -----------------------------------------------------------
    def _colorstr(self, color):
        if len(color) == 1:
            color = color[0]
        if isinstance(color, str):
            if _iscolorstring(color):
                return color
            raise TurtleGraphicsError("bad color string: %s" % str(color))
        try:
            r, g, b = color
        except (TypeError, ValueError):
            raise TurtleGraphicsError("bad color arguments: %s" % str(color))
        if self._colormode == 1.0:
            r = round(255.0 * r)
            g = round(255.0 * g)
            b = round(255.0 * b)
        if not ((0 <= r <= 255) and (0 <= g <= 255) and (0 <= b <= 255)):
            raise TurtleGraphicsError("bad color sequence: %s" % str(color))
        return "#%02x%02x%02x" % (int(r), int(g), int(b))

    def _color(self, cstr):
        if not isinstance(cstr, str):
            return cstr
        if not cstr.startswith("#"):
            return cstr
        if len(cstr) == 7:
            values = [int(cstr[1:3], 16), int(cstr[3:5], 16),
                      int(cstr[5:7], 16)]
        elif len(cstr) == 4:
            values = [16 * int(cstr[1], 16), 16 * int(cstr[2], 16),
                      16 * int(cstr[3], 16)]
        else:
            raise TurtleGraphicsError("bad colorstring: %s" % cstr)
        return (values[0] * self._colormode / 255.0,
                values[1] * self._colormode / 255.0,
                values[2] * self._colormode / 255.0)

    def colormode(self, cmode=None):
        if cmode is None:
            return self._colormode
        if cmode == 1.0:
            self._colormode = 1.0
        elif cmode == 255:
            self._colormode = 255
        else:
            raise TurtleGraphicsError("bad colormode: %s" % str(cmode))

    def bgcolor(self, *args):
        if args:
            color = self._colorstr(args)
            self._bgcolor = color
            try:
                self.cv.configure(bg=color)
            except Exception:
                pass
        return self._color(self._bgcolor)

    def bgpic(self, picname=None):
        if picname is None:
            return self._bgpicname
        if not self._tk_ok:
            raise NotImplementedError(
                "bgpic() requires tkinter image support; this Hydro build has no Tk screen"
            )
        if picname == "nopic":
            self._bgpicname = "nopic"
            return
        try:
            import tkinter
            image = tkinter.PhotoImage(file=picname)
        except Exception:
            raise TurtleGraphicsError("bad image file: %s" % picname)
        self._bgimage = image
        self._bgpicname = picname

    # -- tracing / sizing -------------------------------------------------
    def tracer(self, n=None, delay=None):
        if n is None:
            return self._tracing
        self._tracing = int(n)
        self._updatecounter = 0
        if delay is not None:
            self._delayvalue = int(delay)
        return None

    def delay(self, delay=None):
        if delay is None:
            return self._delayvalue
        self._delayvalue = int(delay)

    def no_animation(self):
        return _NoAnimation(self)

    def update(self):
        for turtle in self.turtles():
            turtle._drawturtle()
        return None

    def _incrementudc(self):
        if self._tracing > 0:
            self._updatecounter += 1
            self._updatecounter = self._updatecounter % self._tracing

    def _window_size(self):
        return (self.cv.winfo_width(), self.cv.winfo_height())

    def window_width(self):
        return self.cv.winfo_width()

    def window_height(self):
        return self.cv.winfo_height()

    def screensize(self, canvwidth=None, canvheight=None, bg=None):
        if canvwidth is None and canvheight is None and bg is None:
            return (self.cv.canvwidth, self.cv.canvheight)
        if canvwidth is not None:
            self.cv.canvwidth = int(canvwidth)
        if canvheight is not None:
            self.cv.canvheight = int(canvheight)
        if bg is not None:
            color = self._colorstr((bg,))
            self._bgcolor = color
            self.cv.configure(bg=color)
        self.cv.reset(self.cv.canvwidth, self.cv.canvheight)
        return None

    def mode(self, mode=None):
        if mode is None:
            return self._mode
        mode = mode.lower()
        if mode not in ["standard", "logo", "world"]:
            raise TurtleGraphicsError("No turtle-graphics-mode %s" % mode)
        self._mode = mode
        for turtle in list(self._turtles):
            turtle._setmode(mode)
            turtle.reset()
        return None

    def setworldcoordinates(self, llx, lly, urx, ury):
        if urx == llx or ury == lly:
            raise TurtleGraphicsError("world coordinates must span an area")
        self._mode = "world"
        self.xscale = self.cv.canvwidth / float(urx - llx)
        self.yscale = self.cv.canvheight / float(ury - lly)
        self.reset()
        return None

    # -- turtle registry --------------------------------------------------
    def turtles(self):
        return self._turtles

    def getcanvas(self):
        return self.cv

    def getshapes(self):
        return list(self._shapes.keys())

    def register_shape(self, name, shape=None):
        if shape is None:
            shape = Shape("image", name)
        elif isinstance(shape, str):
            shape = Shape("image", shape)
        elif isinstance(shape, (tuple, list)):
            shape = Shape("polygon", tuple(shape))
        elif not isinstance(shape, Shape):
            raise TurtleGraphicsError("bad shape argument: %s" % str(shape))
        self._shapes[name] = shape
        return None

    addshape = register_shape

    def clear(self):
        self.cv.delete("all")
        self._bgpicname = "nopic"
        self._bgcolor = "white"
        self._tracing = 1
        self._updatecounter = 0
        for turtle in list(self._turtles):
            # clear() discards the drawing and restores a default turtle.
            turtle.reset()
        try:
            self.cv.configure(bg="white")
        except Exception:
            pass
        return None

    clearscreen = clear

    def reset(self):
        for turtle in list(self._turtles):
            turtle._setmode(self._mode)
            turtle.reset()
        return None

    resetscreen = reset

    def _delete(self, item):
        self.cv.delete(item)

    def _draw_line(self, p1, p2, color, width):
        if color == "" or width <= 0:
            return None
        return self.cv.create_line(p1[0], p1[1], p2[0], p2[1],
                                   fill=color, width=width)

    def _draw_polygon(self, points, fill, outline, width=1):
        if not points:
            return None
        args = []
        for point in points:
            args.append(point[0])
            args.append(point[1])
        return self.cv.create_polygon(*args, fill=fill, outline=outline,
                                      width=width)

    def _draw_oval(self, x, y, radius, color):
        return self.cv.create_oval(x - radius, y - radius,
                                   x + radius, y + radius,
                                   fill=color, outline=color)

    def _draw_text(self, x, y, text, align, font, color):
        anchor = "w"
        if align == "center":
            anchor = "center"
        elif align == "right":
            anchor = "e"
        return self.cv.create_text(x, y, text=text, fill=color, font=font,
                                   anchor=anchor)

    # -- events -----------------------------------------------------------
    def _require_tk(self, name):
        if not self._tk_ok or self._root is None:
            raise NotImplementedError(
                "%s() requires a tkinter screen and event loop; this Hydro "
                "build has no Tk display" % name
            )

    def _event_xy(self, event):
        width = self.cv.winfo_width()
        height = self.cv.winfo_height()
        return (event.x - width / 2.0, height / 2.0 - event.y)

    def _make_xy_handler(self, fun):
        def handler(event):
            xy = self._event_xy(event)
            return fun(xy[0], xy[1])
        return handler

    def _make_noarg_handler(self, fun):
        def handler(event):
            return fun()
        return handler

    def listen(self, xdummy=None, ydummy=None):
        self._require_tk("listen")
        try:
            self._root.focus_force()
        except Exception:
            self.cv.focus_set()
        return None

    def onkey(self, fun, key):
        self._require_tk("onkey")
        sequence = "<KeyPress-%s>" % key
        if fun is None:
            self._root.unbind(sequence)
            if key in self._keys:
                self._keys.remove(key)
            return None
        self._root.bind(sequence, self._make_noarg_handler(fun))
        if key not in self._keys:
            self._keys.append(key)
        return None

    onkeypress = onkey

    def onkeyrelease(self, fun, key):
        self._require_tk("onkeyrelease")
        sequence = "<KeyRelease-%s>" % key
        if fun is None:
            self._root.unbind(sequence)
            return None
        self._root.bind(sequence, self._make_noarg_handler(fun))
        return None

    def onclick(self, fun, btn=1, add=None):
        self._require_tk("onclick")
        sequence = "<Button-%d>" % btn
        if fun is None:
            self.cv.bind(sequence, "")
            return None
        self.cv.bind(sequence, self._make_xy_handler(fun), add)
        return None

    onscreenclick = onclick

    def ontimer(self, fun, t=0):
        self._require_tk("ontimer")
        return self._root.after(t, fun)

    def textinput(self, title, prompt):
        self._require_tk("textinput")
        try:
            import tkinter.simpledialog
            return tkinter.simpledialog.askstring(title, prompt)
        except Exception:
            raise NotImplementedError(
                "textinput() requires tkinter.simpledialog"
            )

    def numinput(self, title, prompt, default=None, minval=None, maxval=None):
        self._require_tk("numinput")
        try:
            import tkinter.simpledialog
            return tkinter.simpledialog.askfloat(title, prompt, initialvalue=default,
                                                 minvalue=minval, maxvalue=maxval)
        except Exception:
            raise NotImplementedError(
                "numinput() requires tkinter.simpledialog"
            )

    def mainloop(self):
        if self._tk_ok and self._root is not None:
            return self._root.mainloop()
        return None

    def save(self, filename, overwrite=False):
        name = str(filename)
        if not (name.endswith(".ps") or name.endswith(".eps")):
            raise ValueError("Unknown file extension: %s" % name)
        if not self._tk_ok:
            raise NotImplementedError(
                "save() requires a Tk canvas with postscript(); this Hydro "
                "build has no Tk screen"
            )
        if not overwrite:
            try:
                handle = open(name, "r")
                handle.close()
                raise FileExistsError("The file '%s' already exists." % name)
            except FileNotFoundError:
                pass
        data = self.cv.postscript()
        handle = open(name, "w")
        try:
            handle.write(data)
        finally:
            handle.close()
        return None

    def _bind_turtle(self, turtle, fun, sequence, add=None):
        self._require_tk("turtle event binding")
        if turtle._icon_item is None:
            turtle._drawturtle()
        if turtle._icon_item is None:
            raise TurtleGraphicsError("turtle is not visible")
        self.cv.tag_bind(turtle._icon_item, sequence,
                         self._make_xy_handler(fun), add)
        return None


class _NoAnimation(object):
    def __init__(self, screen):
        self.screen = screen
        self.previous = None

    def __enter__(self):
        self.previous = self.screen.tracer()
        self.screen.tracer(0)
        return self

    def __exit__(self, exc_type, exc, tb):
        self.screen.tracer(self.previous)
        return False


# ---------------------------------------------------------------------------
# RawTurtle
# ---------------------------------------------------------------------------


class RawTurtle(object):
    """A turtle that draws on an already existing screen/canvas."""

    START_ORIENTATION = {
        "standard": Vec2D(1.0, 0.0),
        "world": Vec2D(1.0, 0.0),
        "logo": Vec2D(0.0, 1.0),
    }

    def __init__(self, canvas=None, shape=_CFG["shape"],
                 undobuffersize=_CFG["undobuffersize"],
                 visible=_CFG["visible"]):
        if canvas is None:
            screen = _getscreen()
        elif isinstance(canvas, TurtleScreen):
            screen = canvas
        elif isinstance(canvas, ScrolledCanvas):
            screen = TurtleScreen(canvas)
        elif hasattr(canvas, "winfo_width") and not isinstance(canvas, _HeadlessCanvas):
            screen = TurtleScreen(canvas)
        else:
            screen = TurtleScreen(canvas)
        self.screen = screen

        self._mode = screen.mode()
        self._angleOffset = 0
        self._angleOrient = 1
        self._fullcircle = 360.0
        self._degreesPerAU = 1.0
        self._position = Vec2D(0.0, 0.0)
        self._orient = Vec2D(1.0, 0.0)
        self._drawing = True
        self._pensize = 1
        self._shown = visible
        self._pencolor = _CFG["pencolor"]
        self._fillcolor = _CFG["fillcolor"]
        self._speed = 3
        self._stretchfactor = (1.0, 1.0)
        self._shearfactor = 0.0
        self._tilt = 0.0
        self._shapetrafo = (1.0, 0.0, 0.0, 1.0)
        self._outlinewidth = 1
        self._resizemode = _CFG["resizemode"]
        self._shape = shape
        self._undobuffersize = undobuffersize
        self._undo = []
        self._items = []
        self._stamp_items = []
        self._icon_item = None
        self._fillpath = None
        self._poly = None
        self._creatingPoly = False

        self._setmode(screen.mode())
        self._reset_navigator()
        self._setDegreesPerAU(360.0)
        screen._turtles.append(self)
        self._drawturtle()

    # -- basic state ------------------------------------------------------
    def _setmode(self, mode=None):
        if mode is None:
            return self._mode
        if mode not in ["standard", "logo", "world"]:
            return
        self._mode = mode
        if mode in ["standard", "world"]:
            self._angleOffset = 0
            self._angleOrient = 1
        else:
            self._angleOffset = self._fullcircle / 4.0
            self._angleOrient = -1
        return None

    def _setDegreesPerAU(self, fullcircle):
        self._fullcircle = fullcircle
        self._degreesPerAU = 360.0 / fullcircle
        if self._mode == "standard":
            self._angleOffset = 0
        else:
            self._angleOffset = fullcircle / 4.0

    def _reset_navigator(self):
        self._position = Vec2D(0.0, 0.0)
        self._orient = Vec2D(self.START_ORIENTATION[self._mode][0],
                             self.START_ORIENTATION[self._mode][1])

    def _capture(self):
        fillpath = None
        if self._fillpath is not None:
            fillpath = list(self._fillpath)
        return (self._position, self._orient, self._drawing,
                self._pencolor, self._fillcolor, self._pensize, self._shown,
                self._speed, self._stretchfactor, self._outlinewidth,
                self._tilt, self._resizemode, self._shearfactor,
                self._shapetrafo, fillpath, self._shape)

    def _restore(self, state):
        self._position = state[0]
        self._orient = state[1]
        self._drawing = state[2]
        self._pencolor = state[3]
        self._fillcolor = state[4]
        self._pensize = state[5]
        self._shown = state[6]
        self._speed = state[7]
        self._stretchfactor = state[8]
        self._outlinewidth = state[9]
        self._tilt = state[10]
        self._resizemode = state[11]
        self._shearfactor = state[12]
        self._shapetrafo = state[13]
        self._fillpath = state[14]
        self._shape = state[15]

    def _commit_undo(self, before, item_start):
        if self._undobuffersize is None:
            return
        new_items = list(self._items[item_start:])
        self._undo.append((before, new_items))
        while len(self._undo) > self._undobuffersize:
            self._undo.pop(0)

    def _track_item(self, item):
        if item is not None:
            self._items.append(item)
        return item

    def _recompute_shapetrafo(self):
        scx, scy = self._stretchfactor
        shf = self._shearfactor
        sa = math.sin(self._tilt)
        ca = math.cos(self._tilt)
        self._shapetrafo = (scx * ca, scy * (shf * ca + sa),
                            -scx * sa, scy * (ca - shf * sa))

    # -- geometry ---------------------------------------------------------
    def _go(self, distance):
        end = Vec2D(self._position[0] + self._orient[0] * distance,
                    self._position[1] + self._orient[1] * distance)
        self._goto(end)

    def _rotate(self, angle):
        before = self._capture()
        start = len(self._items)
        self._orient = self._orient.rotate(angle * self._degreesPerAU)
        self._drawturtle()
        self._commit_undo(before, start)

    def _goto(self, end, draw=True):
        before = self._capture()
        start_index = len(self._items)
        start = self._position
        if draw and self._drawing:
            item = self.screen._draw_line(start, end,
                                          self._pencolor, self._pensize)
            self._track_item(item)
        if self._fillpath is not None:
            self._fillpath.append(end)
        self._position = end
        if self._creatingPoly and self._poly is not None:
            self._poly.append(end)
        self._drawturtle()
        self._commit_undo(before, start_index)

    def pos(self):
        return self._position

    def xcor(self):
        return self._position[0]

    def ycor(self):
        return self._position[1]

    def forward(self, distance):
        self._go(distance)

    def back(self, distance):
        self._go(-distance)

    def right(self, angle):
        self._rotate(-angle)

    def left(self, angle):
        self._rotate(angle)

    def goto(self, x, y=None):
        if y is None:
            end = Vec2D(x[0], x[1])
        else:
            end = Vec2D(x, y)
        self._goto(end)

    def setx(self, x):
        self._goto(Vec2D(x, self._position[1]))

    def sety(self, y):
        self._goto(Vec2D(self._position[0], y))

    def home(self):
        self.goto(0, 0)
        self.setheading(0)

    def teleport(self, x=None, y=None, fill_gap=False):
        was_filling = self.filling()
        if was_filling and not fill_gap:
            self.end_fill()
        new_x = x if x is not None else self._position[0]
        new_y = y if y is not None else self._position[1]
        self._goto(Vec2D(new_x, new_y), draw=False)
        if was_filling and not fill_gap:
            self.begin_fill()
        return None

    def distance(self, x, y=None):
        if y is not None:
            pos = Vec2D(x, y)
        elif isinstance(x, Vec2D):
            pos = x
        elif isinstance(x, tuple) or isinstance(x, list):
            pos = Vec2D(x[0], x[1])
        elif hasattr(x, "_position"):
            pos = x._position
        else:
            raise TurtleGraphicsError("bad distance arguments: %s" % str(x))
        dx = pos[0] - self._position[0]
        dy = pos[1] - self._position[1]
        return math.hypot(dx, dy)

    def towards(self, x, y=None):
        if y is not None:
            pos = Vec2D(x, y)
        elif isinstance(x, Vec2D):
            pos = x
        elif isinstance(x, tuple) or isinstance(x, list):
            pos = Vec2D(x[0], x[1])
        elif hasattr(x, "_position"):
            pos = x._position
        else:
            raise TurtleGraphicsError("bad towards arguments: %s" % str(x))
        dx = pos[0] - self._position[0]
        dy = pos[1] - self._position[1]
        result = round(math.degrees(math.atan2(dy, dx)), 10) % 360.0
        result = result / self._degreesPerAU
        return (self._angleOffset + self._angleOrient * result) % self._fullcircle

    def heading(self):
        result = round(math.degrees(math.atan2(self._orient[1],
                                               self._orient[0])), 10) % 360.0
        result = result / self._degreesPerAU
        return (self._angleOffset + self._angleOrient * result) % self._fullcircle

    def setheading(self, to_angle):
        angle = (to_angle - self.heading()) * self._angleOrient
        full = self._fullcircle
        angle = (angle + full / 2.0) % full - full / 2.0
        self._rotate(angle)

    def circle(self, radius, extent=None, steps=None):
        if extent is None:
            extent = self._fullcircle
        if steps is None:
            frac = abs(extent) / self._fullcircle
            steps = 1 + int(min(11 + abs(radius) / 6.0, 59.0) * frac)
        steps = int(steps)
        if steps < 1:
            steps = 1
        w = 1.0 * extent / steps
        w2 = 0.5 * w
        length = 2.0 * radius * math.sin(math.radians(w2) * self._degreesPerAU)
        if radius < 0:
            length = -length
            w = -w
            w2 = -w2
        self._rotate(w2)
        for _ in range(steps):
            self._go(length)
            self._rotate(w)
        self._rotate(-w2)

    def speed(self, speed=None):
        speed_strings = {
            "fastest": 0,
            "fast": 10,
            "normal": 6,
            "slow": 3,
            "slowest": 1,
        }
        if speed is None:
            return self._speed
        if speed in speed_strings:
            speed = speed_strings[speed]
        elif 0.5 < speed < 10.5:
            speed = int(round(speed))
        else:
            speed = 0
        before = self._capture()
        start = len(self._items)
        self._speed = speed
        self._commit_undo(before, start)

    # -- pen --------------------------------------------------------------
    def _set_pen(self, **pendict):
        before = self._capture()
        start = len(self._items)
        if "shown" in pendict:
            self._shown = pendict["shown"]
        if "pendown" in pendict:
            self._drawing = pendict["pendown"]
        if "pencolor" in pendict:
            self._pencolor = pendict["pencolor"]
        if "fillcolor" in pendict:
            self._fillcolor = pendict["fillcolor"]
        if "pensize" in pendict:
            self._pensize = pendict["pensize"]
        if "speed" in pendict:
            self._speed = pendict["speed"]
        if "resizemode" in pendict:
            self._resizemode = pendict["resizemode"]
        if "stretchfactor" in pendict:
            sf = pendict["stretchfactor"]
            if isinstance(sf, (int, float)):
                sf = (sf, sf)
            self._stretchfactor = sf
        if "shearfactor" in pendict:
            self._shearfactor = pendict["shearfactor"]
        if "outline" in pendict:
            self._outlinewidth = pendict["outline"]
        if "tilt" in pendict:
            self._tilt = pendict["tilt"]
        if ("stretchfactor" in pendict or "tilt" in pendict
                or "shearfactor" in pendict):
            self._recompute_shapetrafo()
        self._drawturtle()
        self._commit_undo(before, start)

    def pen(self, pen=None, **pendict):
        current = {
            "shown": self._shown,
            "pendown": self._drawing,
            "pencolor": self._pencolor,
            "fillcolor": self._fillcolor,
            "pensize": self._pensize,
            "speed": self._speed,
            "resizemode": self._resizemode,
            "stretchfactor": self._stretchfactor,
            "shearfactor": self._shearfactor,
            "outline": self._outlinewidth,
            "tilt": self._tilt,
        }
        if pen is None and not pendict:
            return current
        values = {}
        if isinstance(pen, dict):
            for key in pen:
                values[key] = pen[key]
        for key in pendict:
            values[key] = pendict[key]
        if "pencolor" in values:
            if isinstance(values["pencolor"], tuple):
                values["pencolor"] = self._colorstr((values["pencolor"],))
        if "fillcolor" in values:
            if isinstance(values["fillcolor"], tuple):
                values["fillcolor"] = self._colorstr((values["fillcolor"],))
        self._set_pen(**values)
        return None

    def pensize(self, width=None):
        if width is None:
            return self._pensize
        self._set_pen(pensize=width)

    def penup(self):
        if self._drawing:
            self._set_pen(pendown=False)

    def pendown(self):
        if not self._drawing:
            self._set_pen(pendown=True)

    def isdown(self):
        return self._drawing

    def _colorstr(self, args):
        return self.screen._colorstr(args)

    def _color(self, cstr):
        return self.screen._color(cstr)

    def _cc(self, args):
        if isinstance(args, str):
            return args
        try:
            r, g, b = args
        except (TypeError, ValueError):
            raise TurtleGraphicsError("bad color arguments: %s" % str(args))
        if self.screen._colormode == 1.0:
            r = round(255.0 * r)
            g = round(255.0 * g)
            b = round(255.0 * b)
        if not ((0 <= r <= 255) and (0 <= g <= 255) and (0 <= b <= 255)):
            raise TurtleGraphicsError("bad color sequence: %s" % str(args))
        return "#%02x%02x%02x" % (int(r), int(g), int(b))

    def color(self, *args):
        if args:
            length = len(args)
            if length == 1:
                pcolor = args[0]
                fcolor = args[0]
            elif length == 2:
                pcolor = args[0]
                fcolor = args[1]
            elif length == 3:
                pcolor = args
                fcolor = args
            else:
                raise TurtleGraphicsError("bad color arguments: %s" % str(args))
            pcolor = self._colorstr((pcolor,))
            fcolor = self._colorstr((fcolor,))
            self._set_pen(pencolor=pcolor, fillcolor=fcolor)
        else:
            return (self._color(self._pencolor), self._color(self._fillcolor))

    def pencolor(self, *args):
        if args:
            color = self._colorstr(args)
            if color != self._pencolor:
                self._set_pen(pencolor=color)
        else:
            return self._color(self._pencolor)

    def fillcolor(self, *args):
        if args:
            color = self._colorstr(args)
            if color != self._fillcolor:
                self._set_pen(fillcolor=color)
        else:
            return self._color(self._fillcolor)

    def showturtle(self):
        if not self._shown:
            self._set_pen(shown=True)

    def hideturtle(self):
        if self._shown:
            self._set_pen(shown=False)
            self._drawturtle()

    def isvisible(self):
        return self._shown

    # -- shape ------------------------------------------------------------
    def shape(self, name=None):
        if name is None:
            return self._shape
        if name not in self.screen.getshapes():
            raise TurtleGraphicsError("There is no shape named %s" % name)
        before = self._capture()
        start = len(self._items)
        self._shape = name
        self._drawturtle()
        self._commit_undo(before, start)

    def shapesize(self, stretch_wid=None, stretch_len=None, outline=None):
        if stretch_wid is stretch_len is outline is None:
            return (self._stretchfactor[0], self._stretchfactor[1],
                    self._outlinewidth)
        if ((stretch_wid is not None and stretch_wid == 0)
                or (stretch_len is not None and stretch_len == 0)):
            raise TurtleGraphicsError("stretch_wid/stretch_len must not be zero")
        if stretch_wid is not None:
            if stretch_len is None:
                stretchfactor = (stretch_wid, stretch_wid)
            else:
                stretchfactor = (stretch_wid, stretch_len)
        elif stretch_len is not None:
            stretchfactor = (self._stretchfactor[0], stretch_len)
        else:
            stretchfactor = self._stretchfactor
        if outline is None:
            outline = self._outlinewidth
        self._set_pen(resizemode="user", stretchfactor=stretchfactor,
                      outline=outline)
        return None

    turtlesize = shapesize

    def resizemode(self, rmode=None):
        if rmode is None:
            return self._resizemode
        rmode = rmode.lower()
        if rmode in ["auto", "user", "noresize"]:
            self._set_pen(resizemode=rmode)
        return None

    def shearfactor(self, shear=None):
        if shear is None:
            return self._shearfactor
        self._set_pen(resizemode="user", shearfactor=shear)

    def tiltangle(self, angle=None):
        if angle is None:
            tilt = -math.degrees(self._tilt) * self._angleOrient
            return (tilt / self._degreesPerAU) % self._fullcircle
        tilt = -angle * self._degreesPerAU * self._angleOrient
        tilt = math.radians(tilt) % math.tau
        self._set_pen(resizemode="user", tilt=tilt)
        return None

    def tilt(self, angle):
        self.tiltangle(angle + self.tiltangle())

    def shapetransform(self, t11=None, t12=None, t21=None, t22=None):
        if t11 is t12 is t21 is t22 is None:
            return self._shapetrafo
        m11, m12, m21, m22 = self._shapetrafo
        if t11 is not None:
            m11 = t11
        if t12 is not None:
            m12 = t12
        if t21 is not None:
            m21 = t21
        if t22 is not None:
            m22 = t22
        if m11 * m22 - m12 * m21 == 0:
            raise TurtleGraphicsError("Bad shape transform matrix: must not be singular")
        self._shapetrafo = (m11, m12, m21, m22)
        alfa = math.atan2(-m21, m11) % math.tau
        sa = math.sin(alfa)
        ca = math.cos(alfa)
        a11 = ca * m11 - sa * m21
        a12 = ca * m12 - sa * m22
        a22 = sa * m12 + ca * m22
        self._stretchfactor = (a11, a22)
        self._shearfactor = a12 / a22
        self._tilt = alfa
        self._set_pen(resizemode="user")
        return None

    def _shape_polygon(self):
        shape = self.screen._shapes.get(self._shape)
        if shape is None:
            shape = self.screen._shapes["classic"]
        if shape._type == "polygon":
            return shape._data
        if shape._type == "compound":
            if shape._data:
                return shape._data[0][0]
        return ((-10, -10), (10, -10), (10, 10), (-10, 10))

    def _getshapepoly(self, polygon, compound=False):
        if self._resizemode == "user" or compound:
            t11, t12, t21, t22 = self._shapetrafo
        elif self._resizemode == "auto":
            scale = max(1.0, self._pensize / 5.0)
            t11, t12, t21, t22 = scale, 0.0, 0.0, scale
        else:
            return list(polygon)
        out = []
        for point in polygon:
            out.append((t11 * point[0] + t12 * point[1],
                        t21 * point[0] + t22 * point[1]))
        return out

    def _polytrafo(self, polygon):
        p = self._position
        e0 = self._orient[0]
        e1 = self._orient[1]
        out = []
        for point in polygon:
            out.append((p[0] + (e1 * point[0] + e0 * point[1]),
                        p[1] + (-e0 * point[0] + e1 * point[1])))
        return out

    def get_shapepoly(self):
        shape = self.screen._shapes.get(self._shape)
        if shape is not None and shape._type == "polygon":
            poly = self._getshapepoly(shape._data)
            return tuple(poly)
        return None

    def _drawturtle(self):
        if self._icon_item is not None:
            try:
                self.screen._delete(self._icon_item)
            except Exception:
                pass
            self._icon_item = None
        if not self._shown:
            return
        poly = self._getshapepoly(self._shape_polygon())
        points = self._polytrafo(poly)
        self._icon_item = self.screen._draw_polygon(points, self._fillcolor,
                                                    self._pencolor,
                                                    self._outlinewidth)

    # -- drawing extras ---------------------------------------------------
    def dot(self, size=None, *color):
        if not color:
            if isinstance(size, (str, tuple)):
                color = self._colorstr((size,))
                size = self._pensize + max(self._pensize, 4)
            else:
                color = self._pencolor
                if not size:
                    size = self._pensize + max(self._pensize, 4)
        else:
            if size is None:
                size = self._pensize + max(self._pensize, 4)
            color = self._colorstr(color)
        before = self._capture()
        start = len(self._items)
        item = self.screen._draw_oval(self._position[0], self._position[1],
                                      size / 2.0, color)
        self._track_item(item)
        self._commit_undo(before, start)
        return None

    def stamp(self):
        before = self._capture()
        start = len(self._items)
        poly = self._getshapepoly(self._shape_polygon())
        points = self._polytrafo(poly)
        item = self.screen._draw_polygon(points, self._fillcolor,
                                         self._pencolor, self._outlinewidth)
        self._track_item(item)
        self._stamp_items.append(item)
        self._commit_undo(before, start)
        return item

    def clearstamp(self, stampid):
        if stampid in self._stamp_items:
            self._stamp_items.remove(stampid)
        if stampid in self._items:
            self._items.remove(stampid)
        self.screen._delete(stampid)
        return None

    def clearstamps(self, n=None):
        if n is None:
            to_delete = list(self._stamp_items)
        elif n >= 0:
            to_delete = list(self._stamp_items[:n])
        else:
            to_delete = list(self._stamp_items[n:])
        for item in to_delete:
            self.clearstamp(item)
        return None

    def begin_fill(self):
        self._fillpath = [self._position]
        return None

    def end_fill(self):
        if self._fillpath is None:
            return None
        if len(self._fillpath) > 2:
            before = self._capture()
            start = len(self._items)
            item = self.screen._draw_polygon(self._fillpath,
                                             self._fillcolor, "")
            self._track_item(item)
            self._commit_undo(before, start)
        self._fillpath = None
        return None

    def filling(self):
        return self._fillpath is not None

    def begin_poly(self):
        self._poly = [self._position]
        self._creatingPoly = True

    def end_poly(self):
        self._creatingPoly = False

    def get_poly(self):
        if self._poly is not None:
            return tuple(self._poly)
        return None

    def _write(self, txt, align, font):
        size = 8
        try:
            size = font[1]
        except (TypeError, IndexError):
            size = 8
        item = self.screen._draw_text(self._position[0], self._position[1],
                                      txt, align, font, self._pencolor)
        self._track_item(item)
        width = len(txt) * size * 0.6
        if align == "center":
            endx = self._position[0] + width / 2.0
        elif align == "right":
            endx = self._position[0]
        else:
            endx = self._position[0] + width
        return endx

    def write(self, arg, move=False, align="left",
              font=("Arial", 8, "normal")):
        before = self._capture()
        start = len(self._items)
        txt = str(arg)
        align = align.lower()
        end = self._write(txt, align, font)
        if move:
            self._goto(Vec2D(end, self._position[1]))
        self._commit_undo(before, start)
        return None

    # -- undo / clear -----------------------------------------------------
    def undo(self):
        if not self._undo:
            return None
        entry = self._undo.pop()
        before = entry[0]
        items = entry[1]
        for item in items:
            try:
                self.screen._delete(item)
            except Exception:
                pass
            if item in self._items:
                self._items.remove(item)
            if item in self._stamp_items:
                self._stamp_items.remove(item)
        self._restore(before)
        self._drawturtle()
        return None

    def undobufferentries(self):
        return len(self._undo)

    def setundobuffer(self, size):
        if size is None or size <= 0:
            self._undobuffersize = None
            self._undo = []
        else:
            self._undobuffersize = size
            while len(self._undo) > size:
                self._undo.pop(0)
        return None

    def clear(self):
        for item in list(self._items):
            try:
                self.screen._delete(item)
            except Exception:
                pass
        self._items = []
        self._stamp_items = []
        self._fillpath = None
        self._undo = []
        self._poly = None
        self._creatingPoly = False
        self._drawturtle()
        return None

    def reset(self):
        self._reset_navigator()
        self._drawing = True
        self._pensize = 1
        self._shown = True
        self._pencolor = _CFG["pencolor"]
        self._fillcolor = _CFG["fillcolor"]
        self._speed = 3
        self._stretchfactor = (1.0, 1.0)
        self._shearfactor = 0.0
        self._tilt = 0.0
        self._shapetrafo = (1.0, 0.0, 0.0, 1.0)
        self._outlinewidth = 1
        self._resizemode = _CFG["resizemode"]
        self._setDegreesPerAU(360.0)
        self._setmode(self.screen.mode())
        self.clear()
        return None

    # -- misc -------------------------------------------------------------
    def getscreen(self):
        return self.screen

    def getpen(self):
        return self

    getturtle = getpen

    def clone(self):
        other = RawTurtle(self.screen, shape=self._shape,
                          undobuffersize=self._undobuffersize,
                          visible=self._shown)
        other._position = Vec2D(self._position[0], self._position[1])
        other._orient = Vec2D(self._orient[0], self._orient[1])
        other._drawing = self._drawing
        other._pencolor = self._pencolor
        other._fillcolor = self._fillcolor
        other._pensize = self._pensize
        other._speed = self._speed
        other._stretchfactor = self._stretchfactor
        other._shearfactor = self._shearfactor
        other._tilt = self._tilt
        other._shapetrafo = self._shapetrafo
        other._outlinewidth = self._outlinewidth
        other._resizemode = self._resizemode
        other._drawturtle()
        return other

    def degrees(self, fullcircle=360.0):
        self._setDegreesPerAU(fullcircle)

    def radians(self):
        self._setDegreesPerAU(math.tau)

    def mode(self, mode=None):
        if mode is None:
            return self.screen.mode()
        self.screen.mode(mode)

    def onclick(self, fun, btn=1, add=None):
        return self.screen._bind_turtle(self, fun, "<Button-%d>" % btn, add)

    def ondrag(self, fun, btn=1, add=None):
        return self.screen._bind_turtle(self, fun,
                                        "<B%d-Motion>" % btn, add)

    def onrelease(self, fun, btn=1, add=None):
        return self.screen._bind_turtle(self, fun,
                                        "<ButtonRelease-%d>" % btn, add)

    def fill(self):
        return _FillContext(self)

    def poly(self):
        return _PolyContext(self)

    # aliases
    fd = forward
    bk = back
    backward = back
    rt = right
    lt = left
    position = pos
    setpos = goto
    setposition = goto
    seth = setheading
    pu = penup
    up = penup
    pd = pendown
    down = pendown
    ht = hideturtle
    st = showturtle
    width = pensize


class _FillContext(object):
    def __init__(self, turtle):
        self.turtle = turtle

    def __enter__(self):
        self.turtle.begin_fill()
        return self

    def __exit__(self, exc_type, exc, tb):
        self.turtle.end_fill()
        return False


class _PolyContext(object):
    def __init__(self, turtle):
        self.turtle = turtle

    def __enter__(self):
        self.turtle.begin_poly()
        return self

    def __exit__(self, exc_type, exc, tb):
        self.turtle.end_poly()
        return False


class Turtle(RawTurtle):
    """A RawTurtle that creates the default screen on demand."""

    _screen = None

    def __init__(self, shape=_CFG["shape"],
                 undobuffersize=_CFG["undobuffersize"],
                 visible=_CFG["visible"]):
        screen = _getscreen()
        Turtle._screen = screen
        RawTurtle.__init__(self, screen, shape=shape,
                           undobuffersize=undobuffersize, visible=visible)


RawPen = RawTurtle
Pen = Turtle


class Screen(TurtleScreen):
    """Singleton screen with window-level methods."""

    _instance = None
    _title = _CFG["title"]

    def __new__(cls):
        if cls._instance is None:
            cls._instance = object.__new__(cls)
        return cls._instance

    def __init__(self):
        if getattr(self, "_screen_initialized", False):
            return
        TurtleScreen.__init__(self)
        self._screen_initialized = True
        if _Registry.screen is None:
            _Registry.screen = self

    def setup(self, width=_CFG["width"], height=_CFG["height"],
              startx=None, starty=None):
        if isinstance(width, float) and 0 <= width <= 1:
            width = self.cv.width * width
        if isinstance(height, float) and 0 <= height <= 1:
            height = self.cv.height * height
        width = int(width)
        height = int(height)
        self.cv.set_window_size(width, height)
        if self._tk_ok and self._root is not None:
            try:
                self._root.geometry("%dx%d" % (width, height))
            except Exception:
                pass
        self.cv.update_idletasks()
        return None

    def title(self, titlestring):
        self._title = titlestring
        Screen._title = titlestring
        if self._tk_ok and self._root is not None:
            try:
                self._root.title(titlestring)
            except Exception:
                pass
        return None

    def bye(self):
        if self._root is not None:
            try:
                self._root.destroy()
            except Exception:
                pass
        try:
            self.cv.delete("all")
        except Exception:
            pass
        self._root = None
        self._tk_ok = False
        _Registry.screen = None
        _Registry.pen = None
        Turtle._screen = None
        Screen._instance = None
        return None

    def exitonclick(self):
        if not self._tk_ok or self._root is None:
            return None

        def exitGracefully(x, y):
            self.bye()

        self.onclick(exitGracefully)
        return self.mainloop()


# ---------------------------------------------------------------------------
# Module-level API
# ---------------------------------------------------------------------------


def _getscreen():
    if _Registry.screen is None:
        _Registry.screen = Screen()
    return _Registry.screen


def _getpen():
    if _Registry.pen is None:
        _Registry.pen = Turtle()
    return _Registry.pen


def _screen_or_none():
    return _Registry.screen


# screen functions

def addshape(name, shape=None):
    return _getscreen().register_shape(name, shape)


def bgcolor(*args):
    return _getscreen().bgcolor(*args)


def bgpic(picname=None):
    return _getscreen().bgpic(picname)


def bye():
    screen = _screen_or_none()
    if screen is None:
        return None
    return screen.bye()


def clearscreen():
    return _getscreen().clear()


def colormode(cmode=None):
    return _getscreen().colormode(cmode)


def delay(delay=None):
    return _getscreen().delay(delay)


def exitonclick():
    screen = _screen_or_none()
    if screen is None:
        return None
    return screen.exitonclick()


def getcanvas():
    return _getscreen().getcanvas()


def getshapes():
    return _getscreen().getshapes()


def listen(xdummy=None, ydummy=None):
    return _getscreen().listen(xdummy, ydummy)


def mainloop():
    screen = _screen_or_none()
    if screen is None:
        return None
    return screen.mainloop()


done = mainloop


def mode(mode=None):
    return _getscreen().mode(mode)


def no_animation():
    return _getscreen().no_animation()


def numinput(title, prompt, default=None, minval=None, maxval=None):
    return _getscreen().numinput(title, prompt, default, minval, maxval)


def onkey(fun, key):
    return _getscreen().onkey(fun, key)


def onkeypress(fun, key):
    return _getscreen().onkeypress(fun, key)


def onkeyrelease(fun, key):
    return _getscreen().onkeyrelease(fun, key)


def onscreenclick(fun, btn=1, add=None):
    return _getscreen().onclick(fun, btn, add)


def ontimer(fun, t=0):
    return _getscreen().ontimer(fun, t)


def register_shape(name, shape=None):
    return _getscreen().register_shape(name, shape)


def resetscreen():
    return _getscreen().reset()


def screensize(canvwidth=None, canvheight=None, bg=None):
    return _getscreen().screensize(canvwidth, canvheight, bg)


def save(filename, overwrite=False):
    return _getscreen().save(filename, overwrite)


def setup(width=_CFG["width"], height=_CFG["height"],
          startx=None, starty=None):
    return _getscreen().setup(width, height, startx, starty)


def setworldcoordinates(llx, lly, urx, ury):
    return _getscreen().setworldcoordinates(llx, lly, urx, ury)


def textinput(title, prompt):
    return _getscreen().textinput(title, prompt)


def title(titlestring):
    return _getscreen().title(titlestring)


def tracer(n=None, delay=None):
    return _getscreen().tracer(n, delay)


def turtles():
    return _getscreen().turtles()


def update():
    return _getscreen().update()


def window_height():
    return _getscreen().window_height()


def window_width():
    return _getscreen().window_width()


# turtle functions

def back(distance):
    return _getpen().back(distance)


def backward(distance):
    return _getpen().back(distance)


bk = backward


def begin_fill():
    return _getpen().begin_fill()


def begin_poly():
    return _getpen().begin_poly()


def circle(radius, extent=None, steps=None):
    return _getpen().circle(radius, extent, steps)


def clear():
    return _getpen().clear()


def clearstamp(stampid):
    return _getpen().clearstamp(stampid)


def clearstamps(n=None):
    return _getpen().clearstamps(n)


def clone():
    return _getpen().clone()


def color(*args):
    return _getpen().color(*args)


def degrees(fullcircle=360.0):
    return _getpen().degrees(fullcircle)


def distance(x, y=None):
    return _getpen().distance(x, y)


def dot(size=None, *color):
    return _getpen().dot(size, *color)


def down():
    return _getpen().pendown()


def end_fill():
    return _getpen().end_fill()


def end_poly():
    return _getpen().end_poly()


def fd(distance):
    return _getpen().forward(distance)


def fillcolor(*args):
    return _getpen().fillcolor(*args)


def fill():
    return _getpen().fill()


def filling():
    return _getpen().filling()


def forward(distance):
    return _getpen().forward(distance)


def get_poly():
    return _getpen().get_poly()


def getpen():
    return _getpen()


getturtle = getpen


def getscreen():
    return _getpen().getscreen()


def get_shapepoly():
    return _getpen().get_shapepoly()


def goto(x, y=None):
    return _getpen().goto(x, y)


def heading():
    return _getpen().heading()


def hideturtle():
    return _getpen().hideturtle()


def home():
    return _getpen().home()


def ht():
    return _getpen().hideturtle()


def isdown():
    return _getpen().isdown()


def isvisible():
    return _getpen().isvisible()


def left(angle):
    return _getpen().left(angle)


def lt(angle):
    return _getpen().left(angle)


def onclick(fun, btn=1, add=None):
    return _getpen().onclick(fun, btn, add)


def ondrag(fun, btn=1, add=None):
    return _getpen().ondrag(fun, btn, add)


def onrelease(fun, btn=1, add=None):
    return _getpen().onrelease(fun, btn, add)


def pd():
    return _getpen().pendown()


def pen(pen=None, **pendict):
    return _getpen().pen(pen, **pendict)


def pencolor(*args):
    return _getpen().pencolor(*args)


def pendown():
    return _getpen().pendown()


def pensize(width=None):
    return _getpen().pensize(width)


def penup():
    return _getpen().penup()


def poly():
    return _getpen().poly()


def pos():
    return _getpen().pos()


position = pos


def pu():
    return _getpen().penup()


def radians():
    return _getpen().radians()


def right(angle):
    return _getpen().right(angle)


def reset():
    return _getpen().reset()


def resizemode(rmode=None):
    return _getpen().resizemode(rmode)


def rt(angle):
    return _getpen().right(angle)


def seth(to_angle):
    return _getpen().setheading(to_angle)


def setheading(to_angle):
    return _getpen().setheading(to_angle)


def setpos(x, y=None):
    return _getpen().goto(x, y)


setposition = setpos


def setundobuffer(size):
    return _getpen().setundobuffer(size)


def setx(x):
    return _getpen().setx(x)


def sety(y):
    return _getpen().sety(y)


def shape(name=None):
    return _getpen().shape(name)


def shapesize(stretch_wid=None, stretch_len=None, outline=None):
    return _getpen().shapesize(stretch_wid, stretch_len, outline)


turtlesize = shapesize


def shapetransform(t11=None, t12=None, t21=None, t22=None):
    return _getpen().shapetransform(t11, t12, t21, t22)


def shearfactor(shear=None):
    return _getpen().shearfactor(shear)


def showturtle():
    return _getpen().showturtle()


def speed(speed=None):
    return _getpen().speed(speed)


def st():
    return _getpen().showturtle()


def stamp():
    return _getpen().stamp()


def teleport(x=None, y=None, fill_gap=False):
    return _getpen().teleport(x, y, fill_gap)


def tilt(angle):
    return _getpen().tilt(angle)


def tiltangle(angle=None):
    return _getpen().tiltangle(angle)


def towards(x, y=None):
    return _getpen().towards(x, y)


def undo():
    return _getpen().undo()


def undobufferentries():
    return _getpen().undobufferentries()


def up():
    return _getpen().penup()


def width(width=None):
    return _getpen().pensize(width)


def write(arg, move=False, align="left", font=("Arial", 8, "normal")):
    return _getpen().write(arg, move, align, font)


def xcor():
    return _getpen().xcor()


def ycor():
    return _getpen().ycor()


def write_docstringdict(filename="turtle_docstringdict"):
    docs = {}
    screen_methods = ["bgcolor", "bye", "clear", "colormode", "delay",
                      "exitonclick", "getcanvas", "getshapes", "listen",
                      "mainloop", "mode", "register_shape", "reset",
                      "screensize", "setup", "title", "tracer", "update"]
    turtle_methods = ["back", "circle", "clear", "color", "distance", "dot",
                      "forward", "goto", "heading", "home", "pencolor",
                      "pensize", "reset", "shape", "speed", "stamp", "undo",
                      "write"]
    for name in screen_methods:
        docs["_Screen." + name] = getattr(Screen, name).__doc__
    for name in turtle_methods:
        docs["Turtle." + name] = getattr(Turtle, name).__doc__
    handle = open(filename + ".py", "w")
    handle.write("docsdict = {\n\n")
    keys = list(docs.keys())
    for key in keys:
        handle.write("%r:\n" % key)
        handle.write('        """%s\n""",\n\n' % docs[key])
    handle.write("}\n")
    handle.close()
    return None


__all__ = [
    "ScrolledCanvas", "TurtleScreen", "Screen", "RawTurtle", "Turtle",
    "RawPen", "Pen", "Shape", "Vec2D",
    "addshape", "bgcolor", "bgpic", "bye", "clearscreen", "colormode",
    "delay", "exitonclick", "getcanvas", "getshapes", "listen", "mainloop",
    "mode", "no_animation", "numinput", "onkey", "onkeypress",
    "onkeyrelease", "onscreenclick", "ontimer", "register_shape",
    "resetscreen", "screensize", "save", "setup", "setworldcoordinates",
    "textinput", "title", "tracer", "turtles", "update", "window_height",
    "window_width",
    "back", "backward", "begin_fill", "begin_poly", "bk", "circle",
    "clear", "clearstamp", "clearstamps", "clone", "color", "degrees",
    "distance", "dot", "down", "end_fill", "end_poly", "fd", "fillcolor",
    "fill", "filling", "forward", "get_poly", "getpen", "getscreen",
    "get_shapepoly", "getturtle", "goto", "heading", "hideturtle", "home",
    "ht", "isdown", "isvisible", "left", "lt", "onclick", "ondrag",
    "onrelease", "pd", "pen", "pencolor", "pendown", "pensize", "penup",
    "poly", "pos", "position", "pu", "radians", "right", "reset",
    "resizemode", "rt", "seth", "setheading", "setpos", "setposition",
    "setundobuffer", "setx", "sety", "shape", "shapesize", "shapetransform",
    "shearfactor", "showturtle", "speed", "st", "stamp", "teleport", "tilt",
    "tiltangle", "towards", "turtlesize", "undo", "undobufferentries",
    "up", "width", "write", "xcor", "ycor",
    "write_docstringdict", "done", "Terminator", "TurtleGraphicsError",
]
