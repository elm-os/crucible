SELECT * FROM values('array_string Array(String)', [], [''], ['a'], ['a', 'bc', 'def'], ['héllo ✓'], [repeat('x', 128)], [], ['\x00\r\n', '\xff']) FORMAT Native
