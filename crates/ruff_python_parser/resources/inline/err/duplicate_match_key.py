match x:
    case {"x": 1, "x": 2}: ...
    case {b"x": 1, b"x": 2}: ...
    case {0: 1, 0: 2}: ...
    case {1.0: 1, 1.0: 2}: ...
    case {1.0 + 2j: 1, 1.0 + 2j: 2}: ...
    case {True: 1, True: 2}: ...
    case {None: 1, None: 2}: ...
    case {
    """x
    y
    z
    """: 1,
    """x
    y
    z
    """: 2}: ...
    case {"x": 1, "x": 2, "x": 3}: ...
    case {0: 1, "x": 1, 0: 2, "x": 2}: ...
    case [{"x": 1, "x": 2}]: ...
    case Foo(x=1, y={"x": 1, "x": 2}): ...
    case [Foo(x=1), Foo(x=1, y={"x": 1, "x": 2})]: ...
