SELECT * FROM values('string String', '', 'a', '\x00\xff\r\n', 'héllo ✓ 世界', repeat('x', 127), repeat('y', 128), repeat('z', 16383), repeat('w', 16384)) FORMAT Native
