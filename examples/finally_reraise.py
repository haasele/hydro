try:
    try:
        raise ValueError("reraise-finally")
    finally:
        raise
except ValueError as e:
    print("finally-reraise", e)

try:
    try:
        try:
            raise ValueError("handled")
        except ValueError:
            pass
        finally:
            raise
    except ValueError:
        print("wrong")
except RuntimeError as e:
    print("no-active", e)
