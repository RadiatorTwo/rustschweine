// Exports every function of the open program (address, size, name) as JSON, for
// rustschweine's data/warhogs_functions.json and data/d3d_functions.json. Boundaries and
// names only: nothing of the game's code goes into the file.
//
// Ghidra: Window > Script Manager, add this folder (tools/ghidra) with "Manage Script
// Directories", then run ExportFunctions (category Export).
//@category Export

import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.AddressSetView;
import ghidra.program.model.listing.Function;

import java.io.File;
import java.io.FileWriter;
import java.io.PrintWriter;

public class ExportFunctions extends GhidraScript {
    @Override
    public void run() throws Exception {
        File out = askFile("Save the function list of " + currentProgram.getName(), "Save");
        int n = 0;
        try (PrintWriter w = new PrintWriter(new FileWriter(out))) {
            w.println("[");
            for (Function f : currentProgram.getFunctionManager().getFunctions(true)) {
                AddressSetView body = f.getBody();
                String name = f.getName().replace("\\", "\\\\").replace("\"", "\\\"");
                w.print(n == 0 ? "" : ",\n");
                w.printf("{\"addr\": \"0x%x\", \"size\": %d, \"name\": \"%s\", \"thunk\": %b, \"chunks\": %d}",
                        f.getEntryPoint().getOffset(), body.getNumAddresses(), name, f.isThunk(),
                        body.getNumAddressRanges());
                n++;
            }
            w.println("\n]");
        }
        println(n + " functions written to " + out);
    }
}
