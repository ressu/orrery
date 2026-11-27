# File fixtures

Files that upload and download steps send and compare, by name.

| File | Contents |
|---|---|
| `prusaslicer_cube.gcode` | A small PrusaSlicer 2.7 file: header, two thumbnails (32x32 and 300x300), ten layers, and the slicer's config block. Written by hand to match PrusaSlicer's layout. |
| `plain.gcode` | Three lines of gcode with no slicer comments. |
| `cura_cube.ufp` | A small UFP file, as Cura writes for OctoPrint's UFP plugin: `/3D/model.gcode`, which is `cura_cube.gcode`, and `/Metadata/thumbnail.png`, 64x48. Written by a script to match Cura's layout. |
| `cura_cube.gcode` | The gcode in `cura_cube.ufp`: a Cura 5.6 file with ten layers. |
