;FLAVOR:Marlin
;TIME:930
;Filament used: 1.23456m
;Layer height: 0.2
;MINX:100
;MINY:100
;MINZ:0.2
;MAXX:120
;MAXY:120
;MAXZ:2
;TARGET_MACHINE.NAME:Creality Ender-3
;Generated with Cura_SteamEngine 5.6.0
M140 S60
M105
M190 S60
M104 S215
M105
M109 S215
M82 ;absolute extrusion mode
G28
;LAYER_COUNT:10
;LAYER:0
G1 F1500 E-6.5
G0 X100 Y100 Z0.2
G1 X120 Y100 E1
;LAYER:1
G0 Z0.4
G1 X120 Y120 E2
;TIME_ELAPSED:930.0
M140 S0
M107
M82 ;absolute extrusion mode
M104 S0
;End of Gcode
;SETTING_3 {"global_quality": "[general]\\nversion = 4\\nname = Standard Quality #2\\n"}
