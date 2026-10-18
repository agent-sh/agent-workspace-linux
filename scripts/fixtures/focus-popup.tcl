wm overrideredirect . 1
wm title . "Focus smoke"
if {[llength $argv] > 2 && [lindex $argv 2] eq "managed"} {
    wm overrideredirect . 0
    wm title . "Managed focus smoke"
}
wm geometry . 400x200+40+40
entry .input
pack .input -fill x
focus .input
set child [open [lindex $argv 1] w]
puts $child [winfo id .input]
close $child
bind .input <Return> {
    set output [open [lindex $argv 0] w]
    puts $output [.input get]
    close $output
}
