extends Node
## A real key event (Input.parse_input_event) reaches the game through
## TremInputRelay (game.rs): single player on ATCS, F12 (unbound by default) bound to a cvar.
## godot --path godot res://tests/input_relay_check.tscn
var main: Node
var game
var hud

func _ready() -> void:
	main = load("res://main.tscn").instantiate()
	add_child(main)
	game = main.get_node("Game")
	game.bots_per_team = 0
	hud = main.get_node("HUD")
	_run()

func _wait(secs: float) -> void:
	await get_tree().create_timer(secs).timeout

func _key(code: Key) -> void:
	for pressed in [true, false]:
		var ev := InputEventKey.new()
		ev.keycode = code
		ev.physical_keycode = code
		ev.pressed = pressed
		Input.parse_input_event(ev)
		await get_tree().process_frame
		await get_tree().process_frame

func _run() -> void:
	await _wait(1.0)
	print("relay: node ", "present" if main.has_node("Game/InputRelay") else "missing")
	game.exec("set net_enabled 0\nmap atcs")
	while not (game.has_world() and not game.is_loading()):
		await _wait(0.25)
	await _wait(1.0)
	hud.close_menu()
	await _wait(0.5)
	game.exec("set relaycheck 0\nbind F12 \"set relaycheck 1\"")
	await _key(KEY_F12)
	await _wait(0.2)
	var got: String = game.exec("relaycheck")
	print("relay: after F12: ", got.replace("\n", " "))
	print("relay: %s" % ("PASS" if got.contains("\"1") else "FAIL"))
	game.exec("unbind F12")
	get_tree().quit(0)
