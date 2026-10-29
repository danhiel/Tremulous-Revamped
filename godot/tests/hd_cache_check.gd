extends Node
## The 4K pack's VRAM over map changes (hdtex.rs new_map): loads several
## maps in a row and prints the texture memory after each one.
## HDCHECK_PATH=<pack dir> godot --path godot res://tests/hd_cache_check.tscn
## (r_hdPath is an archived cvar: the path stays in autogen.cfg; set it
## back after). HDCHECK_MAPS="a b ..." other maps, HDCHECK_SETTLE=<s> stay
## on each map (its sound pathing bake runs meanwhile), HDCHECK_LOG=<file>
## lines appended at once. TREM_HD_DEBUG=1 prints what each map change kept
## and dropped; TREM_CRASH_LOG=<file> records access violations
## (trem_audio.c). transit then atcs at once was the pathing bake crash.
var main: Node
var game
var hud

const MAPS := ["atcs", "nexus6", "karith", "transit", "atcs"]

func _ready() -> void:
	main = load("res://main.tscn").instantiate()
	add_child(main)
	game = main.get_node("Game")
	game.bots_per_team = 0
	hud = main.get_node("HUD")
	_run()

func _wait(secs: float) -> void:
	await get_tree().create_timer(secs).timeout

## print, and append to HDCHECK_LOG at once (a killed run loses buffered stdout)
func _log(s: String) -> void:
	print(s)
	var path := OS.get_environment("HDCHECK_LOG")
	if path == "":
		return
	var f := FileAccess.open(path, FileAccess.READ_WRITE if FileAccess.file_exists(path) else FileAccess.WRITE)
	if f:
		f.seek_end()
		f.store_line(s)
		f.close()

func _vram_mb() -> int:
	return int(Performance.get_monitor(Performance.RENDER_TEXTURE_MEM_USED) / 1048576.0)

func _run() -> void:
	await _wait(1.0)
	# single player: no listen server to join (has_world waits for it), no firewall check
	game.exec("set net_enabled 0")
	if OS.get_environment("HDCHECK_PATH") != "":
		game.exec("set r_hdPath \"%s\"" % OS.get_environment("HDCHECK_PATH"))
	await _wait(1.0)
	_log("hdcache: start texture VRAM %d MB" % _vram_mb())
	var maps: Array = MAPS
	if OS.get_environment("HDCHECK_MAPS") != "":
		maps = OS.get_environment("HDCHECK_MAPS").split(" ", false)
	for m in maps:
		var t0 := Time.get_ticks_msec()
		game.exec("map " + m)
		while not (game.has_world() and not game.is_loading()):
			if Time.get_ticks_msec() - t0 > 120000:
				_log("hdcache: %s did not load in 120 s (world %s, loading %s)" % [m, game.has_world(), game.is_loading()])
				get_tree().quit(1)
				return
			await _wait(0.25)
		var load_ms := Time.get_ticks_msec() - t0
		hud.close_menu()
		# freed nodes go at the end of a frame; let a few pass
		await _wait(2.0)
		_log("hdcache: %s loaded in %d ms, texture VRAM %d MB" % [m, load_ms, _vram_mb()])
		# HDCHECK_SETTLE=<s>: stay on each map that long (its sound pathing bake runs meanwhile)
		if OS.get_environment("HDCHECK_SETTLE") != "":
			await _wait(float(OS.get_environment("HDCHECK_SETTLE")))
			_log("hdcache: %s stayed %s s" % [m, OS.get_environment("HDCHECK_SETTLE")])
	get_tree().quit()
