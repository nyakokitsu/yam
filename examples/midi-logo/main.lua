local colors = {}
for i = 1, device.logo_led_count do
  colors[i] = "#181030"
end
alice.logo.set(colors)
alice.sleep(1000)
alice.logo.off()
