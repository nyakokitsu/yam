if device.model ~= "yandexmini_1" then
  alice.off()
  return
end

alice.set({
  "#FF0000", "#FF7A00", "#00FF00",
  "#00FFFF", "#0000FF", "#FF00FF",
})
alice.sleep(1200)

alice.play("patterns/aurora.led")
alice.sleep(2500)

alice.play_system("note_1.led")
alice.sleep(1800)

alice.off()
